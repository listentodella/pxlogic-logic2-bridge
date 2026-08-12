use chrono::Utc;
use serde::Deserialize;

use crate::{
    decode::{AnalyzerDecodeSettings, I2cDecodeSettings, SpiDecodeSettings, UartDecodeSettings},
    decoder_backend::DecoderBackendKind,
    error::{CoreError, Result},
    models::{
        CaptureData, CaptureMetadata, CaptureMode, CaptureSettings, CaptureTriggerKind,
        PwmConfiguration, PwmSettings,
    },
    protocol,
};

#[derive(Debug, Clone, Deserialize)]
pub struct DemoAnalyzerSettings {
    pub backend: DecoderBackendKind,
    pub settings: AnalyzerDecodeSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpioTiming {
    pub mode: u32,
    pub div: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureProfile {
    pub stream_mode: bool,
    pub filter: u8,
    pub clock_edge: u8,
    pub ext_trigger_mode: u32,
    pub trigger_out_enable: u32,
    pub trigger_zero: u32,
    pub trigger_one: u32,
    pub trigger_rise: u32,
    pub trigger_fall: u32,
    pub trigger_pos: u32,
    pub vth_volts: f64,
}

impl Default for CaptureProfile {
    fn default() -> Self {
        Self {
            stream_mode: false,
            filter: 0,
            clock_edge: 0,
            ext_trigger_mode: 0,
            trigger_out_enable: 0,
            trigger_zero: 0,
            trigger_one: 0,
            trigger_rise: 0,
            trigger_fall: 0,
            trigger_pos: 64,
            vth_volts: 2.0,
        }
    }
}

pub fn capture_profile_from_settings(
    settings: &CaptureSettings,
    channel_count: u8,
) -> Result<CaptureProfile> {
    const MAX_THRESHOLD_VOLTS: f64 = 2.0 * 3.334;
    if !settings.threshold_volts.is_finite()
        || !(0.0..=MAX_THRESHOLD_VOLTS).contains(&settings.threshold_volts)
    {
        return Err(CoreError::Decode(format!(
            "threshold voltage must be between 0 and {MAX_THRESHOLD_VOLTS:.3} V"
        )));
    }
    let enabled_channels = resolve_enabled_channels(channel_count, &settings.enabled_channels)?;
    let mut profile = CaptureProfile {
        stream_mode: matches!(settings.mode, CaptureMode::Stream),
        filter: u8::from(settings.glitch_filter_enabled),
        clock_edge: u8::from(settings.clock_edge),
        ext_trigger_mode: settings.external_trigger_mode.into(),
        trigger_out_enable: u32::from(settings.trigger_out_enabled),
        vth_volts: settings.threshold_volts,
        ..CaptureProfile::default()
    };

    if settings.trigger_enabled {
        if !enabled_channels.contains(&settings.trigger_channel) {
            return Err(CoreError::Decode(format!(
                "trigger channel D{} is not enabled",
                settings.trigger_channel
            )));
        }
        let trigger_bit = 1u32
            .checked_shl(u32::from(settings.trigger_channel))
            .ok_or_else(|| CoreError::Decode("trigger channel mask overflow".to_string()))?;
        match settings.trigger_kind {
            CaptureTriggerKind::Rising => profile.trigger_rise = trigger_bit,
            CaptureTriggerKind::Falling => profile.trigger_fall = trigger_bit,
            CaptureTriggerKind::High => profile.trigger_one = trigger_bit,
            CaptureTriggerKind::Low => profile.trigger_zero = trigger_bit,
        }
        let enabled_mask = enabled_channel_mask(channel_count, &enabled_channels)?;
        // PXView passes all simple trigger conditions as bitmasks. Rejecting
        // disabled channels avoids silently programming an unreachable condition.
        if (settings.trigger_high_mask | settings.trigger_low_mask) & !enabled_mask != 0 {
            return Err(CoreError::Decode(
                "trigger conditions include a disabled channel".to_string(),
            ));
        }
        profile.trigger_one |= settings.trigger_high_mask;
        profile.trigger_zero |= settings.trigger_low_mask;
    }

    Ok(profile)
}

/// PXView `set_trigger()` computes the requested pre-trigger sample count from
/// the capture ratio, then clamps it to the FPGA's per-channel memory depth.
pub fn pxview_trigger_position(
    settings: &CaptureSettings,
    sample_count: u64,
    channel_count: u8,
    hardware_depth_bits: u64,
) -> Result<u32> {
    if settings.trigger_position_percent > 100 {
        return Err(CoreError::Decode(format!(
            "trigger position must be between 0 and 100%, got {}%",
            settings.trigger_position_percent
        )));
    }
    if channel_count == 0 {
        return Err(CoreError::InvalidChannelCount(channel_count));
    }

    const PXLOGIC_ATOMIC_SAMPLES: u64 = 64;
    const SAMPLES_ALIGN: u64 = 1023;
    let requested = (u128::from(sample_count)
        .saturating_mul(u128::from(settings.trigger_position_percent))
        / 100)
        .min(u128::from(u64::MAX)) as u64;
    let channel_depth = (hardware_depth_bits / u64::from(channel_count)) & !SAMPLES_ALIGN;
    let maximum_percent = if matches!(settings.mode, CaptureMode::Stream) {
        10
    } else {
        90
    };
    let maximum = channel_depth.saturating_mul(maximum_percent) / 100;
    let clamped = requested.max(PXLOGIC_ATOMIC_SAMPLES).min(maximum);
    u32::try_from(clamped.min(u64::from(u32::MAX))).map_err(|_| CoreError::InvalidCaptureDuration)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisterWrite {
    pub addr: u32,
    pub value: u32,
}

/// Mirrors PXView's `SR_CONF_PWM0_FREQ` / `SR_CONF_PWM0_DUTY` register path.
/// The FPGA output is disabled while period and high-time registers change.
pub fn build_pxview_pwm0_register_script(
    settings: &PwmSettings,
) -> Result<(PwmConfiguration, Vec<RegisterWrite>)> {
    if !settings.frequency_hz.is_finite()
        || !(1.0..=protocol::PWM_MAX_FREQUENCY_HZ).contains(&settings.frequency_hz)
    {
        return Err(CoreError::Decode(format!(
            "PWM0 frequency must be between 1 and {} Hz",
            protocol::PWM_MAX_FREQUENCY_HZ as u64
        )));
    }
    if !settings.duty_percent.is_finite() || !(0.0..=100.0).contains(&settings.duty_percent) {
        return Err(CoreError::Decode(
            "PWM0 duty must be between 0 and 100%".to_string(),
        ));
    }

    let period_ticks = (f64::from(protocol::PWM_CLOCK_HZ) / settings.frequency_hz) as u32;
    if period_ticks == 0 {
        return Err(CoreError::Decode(
            "PWM0 frequency produces a zero clock divisor".to_string(),
        ));
    }
    let high_ticks = (f64::from(period_ticks) * settings.duty_percent / 100.0) as u32;
    let configuration = PwmConfiguration {
        enabled: settings.enabled,
        requested_frequency_hz: settings.frequency_hz,
        requested_duty_percent: settings.duty_percent,
        effective_frequency_hz: f64::from(protocol::PWM_CLOCK_HZ) / f64::from(period_ticks),
        effective_duty_percent: f64::from(high_ticks) * 100.0 / f64::from(period_ticks),
        period_ticks,
        high_ticks,
    };
    let script = vec![
        RegisterWrite {
            addr: protocol::REG_PWM0_ENABLE,
            value: 0,
        },
        RegisterWrite {
            addr: protocol::REG_PWM0_PERIOD,
            value: period_ticks - 1,
        },
        RegisterWrite {
            addr: protocol::REG_PWM0_HIGH,
            value: high_ticks.wrapping_sub(1),
        },
        RegisterWrite {
            addr: protocol::REG_PWM0_ENABLE,
            value: u32::from(settings.enabled),
        },
    ];
    Ok((configuration, script))
}

pub fn supported_samplerates() -> &'static [u64] {
    &[
        2_000,
        5_000,
        10_000,
        20_000,
        40_000,
        50_000,
        100_000,
        200_000,
        400_000,
        500_000,
        1_000_000,
        2_000_000,
        4_000_000,
        5_000_000,
        6_250_000,
        10_000_000,
        20_000_000,
        25_000_000,
        50_000_000,
        100_000_000,
        125_000_000,
        200_000_000,
        250_000_000,
        400_000_000,
        500_000_000,
        800_000_000,
        1_000_000_000,
    ]
}

pub fn gpio_timing_for_samplerate(sample_rate_hz: u64) -> Result<GpioTiming> {
    let timing = match sample_rate_hz {
        1_000_000_000 => GpioTiming { mode: 0, div: 0 },
        500_000_000 => GpioTiming { mode: 1, div: 0 },
        250_000_000 => GpioTiming { mode: 2, div: 0 },
        125_000_000 => GpioTiming { mode: 3, div: 0 },
        800_000_000 => GpioTiming { mode: 4, div: 0 },
        400_000_000 => GpioTiming { mode: 5, div: 0 },
        200_000_000 => GpioTiming { mode: 6, div: 0 },
        100_000_000 => GpioTiming { mode: 7, div: 0 },
        50_000_000 => GpioTiming { mode: 7, div: 1 },
        25_000_000 => GpioTiming { mode: 7, div: 3 },
        20_000_000 => GpioTiming { mode: 7, div: 4 },
        10_000_000 => GpioTiming { mode: 7, div: 9 },
        6_250_000 => GpioTiming { mode: 7, div: 15 },
        5_000_000 => GpioTiming { mode: 7, div: 19 },
        4_000_000 => GpioTiming { mode: 7, div: 24 },
        2_000_000 => GpioTiming { mode: 7, div: 49 },
        1_000_000 => GpioTiming { mode: 7, div: 99 },
        500_000 => GpioTiming { mode: 7, div: 199 },
        400_000 => GpioTiming { mode: 7, div: 249 },
        200_000 => GpioTiming { mode: 7, div: 499 },
        100_000 => GpioTiming { mode: 7, div: 999 },
        50_000 => GpioTiming {
            mode: 7,
            div: 1_999,
        },
        40_000 => GpioTiming {
            mode: 7,
            div: 2_499,
        },
        20_000 => GpioTiming {
            mode: 7,
            div: 4_999,
        },
        10_000 => GpioTiming {
            mode: 7,
            div: 9_999,
        },
        5_000 => GpioTiming {
            mode: 7,
            div: 19_999,
        },
        2_000 => GpioTiming {
            mode: 7,
            div: 49_999,
        },
        other => return Err(CoreError::InvalidSamplerate(other)),
    };
    Ok(timing)
}

pub fn unitsize_for_channel_count(channel_count: u8) -> Result<u8> {
    match channel_count {
        1..=8 => Ok(1),
        9..=16 => Ok(2),
        17..=32 => Ok(4),
        _ => Err(CoreError::InvalidChannelCount(channel_count)),
    }
}

pub fn capture_channel_mask(channel_count: u8) -> Result<u32> {
    match channel_count {
        1..=31 => Ok((1u32 << channel_count) - 1),
        32 => Ok(u32::MAX),
        _ => Err(CoreError::InvalidChannelCount(channel_count)),
    }
}

/// Resolves the physical channel order used by PXView's `en_ch_num()` and
/// `en_ch_num_mask()`. An empty list is the legacy contiguous D0..Dn form.
pub fn resolve_enabled_channels(channel_count: u8, enabled_channels: &[u8]) -> Result<Vec<u8>> {
    capture_channel_mask(channel_count)?;
    if enabled_channels.is_empty() {
        return Ok((0..channel_count).collect());
    }

    let mut resolved = enabled_channels.to_vec();
    resolved.sort_unstable();
    for channels in resolved.windows(2) {
        if channels[0] == channels[1] {
            return Err(CoreError::Decode(format!(
                "enabled channel D{} is listed more than once",
                channels[0]
            )));
        }
    }
    if let Some(channel) = resolved
        .iter()
        .copied()
        .find(|channel| *channel >= channel_count)
    {
        return Err(CoreError::Decode(format!(
            "enabled channel D{channel} is outside the physical channel range 0..{}",
            channel_count.saturating_sub(1)
        )));
    }
    Ok(resolved)
}

pub fn enabled_channel_mask(channel_count: u8, enabled_channels: &[u8]) -> Result<u32> {
    resolve_enabled_channels(channel_count, enabled_channels)?
        .into_iter()
        .try_fold(0u32, |mask, channel| {
            1u32.checked_shl(u32::from(channel))
                .map(|bit| mask | bit)
                .ok_or_else(|| CoreError::Decode("enabled channel mask overflow".to_string()))
        })
}

pub fn sample_count_from_duration(
    sample_rate_hz: u64,
    duration_ms: u64,
    decode_cross: bool,
) -> Result<u64> {
    if duration_ms == 0 {
        return Err(CoreError::InvalidCaptureDuration);
    }
    let mut samples = (u128::from(sample_rate_hz) * u128::from(duration_ms)).div_ceil(1000);
    if samples == 0 {
        samples = 1;
    }
    if decode_cross {
        samples = samples.div_ceil(64) * 64;
    }
    Ok(samples
        .try_into()
        .map_err(|_| CoreError::InvalidCaptureDuration)?)
}

pub fn sample_count_from_settings(settings: &CaptureSettings, unitsize: u8) -> Result<u64> {
    match settings.mode {
        CaptureMode::Buffer => sample_count_from_duration(
            settings.sample_rate_hz,
            settings.duration_ms,
            settings.decode_cross,
        ),
        CaptureMode::Stream => {
            let buffer_bytes = settings
                .buffer_size_mb
                .max(1)
                .checked_mul(1024 * 1024)
                .ok_or(CoreError::InvalidCaptureDuration)?;
            let samples = buffer_bytes / u64::from(unitsize);
            Ok(samples.max(1))
        }
    }
}

pub fn align_cross_sample_count(sample_count: u64) -> Result<u64> {
    sample_count
        .div_ceil(64)
        .checked_mul(64)
        .ok_or(CoreError::InvalidCaptureDuration)
}

pub fn pxview_cross_raw_byte_count(sample_count: u64, channel_count: u8) -> Result<u64> {
    capture_channel_mask(channel_count)?;
    sample_count
        .checked_mul(u64::from(channel_count))
        .map(|bits| bits.div_ceil(8))
        .ok_or(CoreError::InvalidCaptureDuration)
}

pub fn pxview_capture_transfer_size(
    sample_rate_hz: u64,
    channel_count: u8,
    superspeed: bool,
) -> Result<usize> {
    const ALIGNMENT: u64 = 4096;
    const USB2_BITS_PER_SECOND: u64 = 480_000_000;
    const USB3_TRANSFER_CAP: u64 = 4 * 1024 * 1024;

    if !(1..=32).contains(&channel_count) {
        return Err(CoreError::InvalidChannelCount(channel_count));
    }

    let channels = u64::from(channel_count);
    let per_channel_10ms_bytes = (sample_rate_hz / 100 / 8).max(1);
    let per_channel_aligned = align_up(per_channel_10ms_bytes, ALIGNMENT)?;
    let usb_buffer_cap = if superspeed {
        USB3_TRANSFER_CAP
    } else {
        align_up(USB2_BITS_PER_SECOND / 100 / 8, ALIGNMENT)?
    };

    let candidate = per_channel_aligned
        .checked_mul(channels)
        .ok_or_else(|| CoreError::Decode("transfer size overflow".to_string()))?;
    let transfer_size = if candidate > usb_buffer_cap {
        let per_channel_blocks = (usb_buffer_cap / channels / ALIGNMENT).max(1);
        per_channel_blocks
            .checked_mul(ALIGNMENT)
            .and_then(|size| size.checked_mul(channels))
            .ok_or_else(|| CoreError::Decode("transfer size overflow".to_string()))?
    } else {
        candidate
    };

    if transfer_size > u64::from(u32::MAX) {
        return Err(CoreError::Decode(
            "transfer size does not fit hardware register".to_string(),
        ));
    }

    usize::try_from(transfer_size)
        .map_err(|_| CoreError::Decode("transfer size does not fit usize".to_string()))
}

fn align_up(value: u64, alignment: u64) -> Result<u64> {
    if alignment == 0 {
        return Err(CoreError::Decode("alignment must be non-zero".to_string()));
    }
    value
        .div_ceil(alignment)
        .checked_mul(alignment)
        .ok_or_else(|| CoreError::Decode("aligned value overflow".to_string()))
}

pub fn build_capture_register_script(
    transfer_size: u32,
    target_raw_bytes: u64,
    channel_count: u8,
    sample_rate_hz: u64,
    profile: CaptureProfile,
) -> Result<Vec<RegisterWrite>> {
    let enabled_channels = resolve_enabled_channels(channel_count, &[])?;
    build_capture_register_script_for_channels(
        transfer_size,
        target_raw_bytes,
        channel_count,
        &enabled_channels,
        sample_rate_hz,
        profile,
    )
}

pub fn build_capture_register_script_for_channels(
    transfer_size: u32,
    target_raw_bytes: u64,
    output_channel_count: u8,
    enabled_channels: &[u8],
    sample_rate_hz: u64,
    profile: CaptureProfile,
) -> Result<Vec<RegisterWrite>> {
    if transfer_size == 0 {
        return Err(CoreError::Decode(
            "transfer size must be non-zero".to_string(),
        ));
    }

    let enabled_channels = resolve_enabled_channels(output_channel_count, enabled_channels)?;
    let enabled_channel_count = u8::try_from(enabled_channels.len())
        .map_err(|_| CoreError::InvalidChannelCount(output_channel_count))?;
    let channel_mask = enabled_channel_mask(output_channel_count, &enabled_channels)?;
    let target_total = target_raw_bytes
        .checked_add(u64::from(transfer_size))
        .ok_or_else(|| CoreError::Decode("target byte count overflow".to_string()))?;
    let target_low = target_total as u32;
    let target_high = (target_total >> 32) as u32;
    let stream_mask = if profile.stream_mode {
        protocol::STREAM_MODE_BIT
    } else {
        0
    };
    let stream_enable_flags = protocol::STREAM_ENABLE_FLAGS_BASE | stream_mask;
    let stream_enable_pulse_flags = stream_enable_flags | protocol::STREAM_ENABLE_PULSE_FLAG;
    let stream_run_flags =
        stream_mask | (u32::from(profile.filter) << protocol::STREAM_FILTER_SHIFT);
    let gpio_timing = gpio_timing_for_samplerate(sample_rate_hz)?;
    let gpio_mode =
        gpio_timing.mode | (u32::from(profile.clock_edge) << protocol::STREAM_FILTER_SHIFT);
    let pwm_max = 120_000_000u32 / 10_000u32;
    let threshold_vth = ((profile.vth_volts * 0.5 / 3.334) * f64::from(pwm_max)) as u32;

    Ok(vec![
        RegisterWrite {
            addr: protocol::REG_THRESHOLD_PWM_MAX,
            value: pwm_max,
        },
        RegisterWrite {
            addr: protocol::REG_THRESHOLD_VALUE,
            value: threshold_vth,
        },
        // A logic capture must never inherit an output left enabled by an
        // earlier PXView/PWM session. PXView clears both outputs here too.
        RegisterWrite {
            addr: protocol::REG_PWM0_ENABLE,
            value: 0,
        },
        RegisterWrite {
            addr: protocol::REG_PWM1_ENABLE,
            value: 0,
        },
        RegisterWrite {
            addr: protocol::REG_STREAM_CHANNEL_ENABLE,
            value: 0,
        },
        RegisterWrite {
            addr: protocol::REG_STREAM_CONTROL,
            value: stream_enable_flags,
        },
        RegisterWrite {
            addr: protocol::REG_STREAM_CONTROL,
            value: stream_enable_pulse_flags,
        },
        RegisterWrite {
            addr: protocol::REG_STREAM_CONTROL,
            value: stream_enable_flags,
        },
        RegisterWrite {
            addr: protocol::REG_STREAM_START,
            value: protocol::STREAM_STOP_FLAGS,
        },
        RegisterWrite {
            addr: protocol::REG_STREAM_TRANSFER_SIZE,
            value: transfer_size,
        },
        RegisterWrite {
            addr: protocol::REG_STREAM_DMA_SIZE,
            value: transfer_size,
        },
        RegisterWrite {
            addr: protocol::REG_CAPTURE_BYTES_LOW,
            value: target_low,
        },
        RegisterWrite {
            addr: protocol::REG_CAPTURE_BYTES_HIGH,
            value: target_high,
        },
        RegisterWrite {
            addr: protocol::REG_EXT_TRIGGER_MODE,
            value: profile.ext_trigger_mode,
        },
        RegisterWrite {
            addr: protocol::REG_TRIGGER_OUT_ENABLE,
            value: profile.trigger_out_enable,
        },
        RegisterWrite {
            addr: protocol::REG_GPIO_MODE,
            value: gpio_mode,
        },
        RegisterWrite {
            addr: protocol::REG_GPIO_DIV,
            value: gpio_timing.div,
        },
        RegisterWrite {
            addr: protocol::REG_CAPTURE_CHANNEL_COUNT,
            value: u32::from(enabled_channel_count),
        },
        RegisterWrite {
            addr: protocol::REG_CAPTURE_TRIGGER_POS,
            value: profile.trigger_pos,
        },
        RegisterWrite {
            addr: protocol::REG_BLOCK_START,
            value: 0,
        },
        RegisterWrite {
            addr: protocol::REG_STREAM_CHANNEL_ENABLE,
            value: channel_mask,
        },
        RegisterWrite {
            addr: protocol::REG_STREAM_CONTROL,
            value: stream_run_flags,
        },
        RegisterWrite {
            addr: protocol::REG_TRIGGER_ZERO,
            value: profile.trigger_zero,
        },
        RegisterWrite {
            addr: protocol::REG_TRIGGER_ONE,
            value: profile.trigger_one,
        },
        RegisterWrite {
            addr: protocol::REG_TRIGGER_RISE,
            value: profile.trigger_rise,
        },
        RegisterWrite {
            addr: protocol::REG_TRIGGER_FALL,
            value: profile.trigger_fall,
        },
        RegisterWrite {
            addr: protocol::REG_STREAM_START,
            value: protocol::STREAM_START_FLAGS,
        },
    ])
}

pub fn decode_cross_data(channel_count: u8, input: &[u8]) -> Result<Vec<u8>> {
    decode_cross_data_with_map(channel_count, input, None, false)
}

/// Converts PXView `LA_CROSS_DATA` lanes into interleaved sample words while
/// preserving the physical channel numbers selected by the hardware mask.
pub fn decode_cross_data_to_physical_channels(
    output_channel_count: u8,
    enabled_channels: &[u8],
    input: &[u8],
    reverse_bits_in_word: bool,
) -> Result<Vec<u8>> {
    let enabled_channels = resolve_enabled_channels(output_channel_count, enabled_channels)?;
    let unitsize = usize::from(unitsize_for_channel_count(output_channel_count)?);
    let stripe_bytes = enabled_channels.len() * 8;
    if input.len() % stripe_bytes != 0 {
        return Err(CoreError::Decode(
            "cross-lane input is not stripe-aligned".to_string(),
        ));
    }

    let stripe_count = input.len() / stripe_bytes;
    let output_capacity = stripe_count
        .checked_mul(64)
        .and_then(|samples| samples.checked_mul(unitsize))
        .ok_or(CoreError::InvalidCaptureDuration)?;
    let mut output = vec![0; output_capacity];
    let mut output_offset = 0;
    for stripe in input.chunks_exact(stripe_bytes) {
        for sample_block in 0..8 {
            let source_byte = if reverse_bits_in_word {
                7 - sample_block
            } else {
                sample_block
            };
            let mut output_byte_matrices = [0u64; 4];
            for (raw_lane, physical_channel) in enabled_channels.iter().copied().enumerate() {
                let mut lane_samples = stripe[raw_lane * 8 + source_byte];
                if reverse_bits_in_word {
                    lane_samples = lane_samples.reverse_bits();
                }
                let output_byte = usize::from(physical_channel / 8);
                let channel_in_byte = usize::from(physical_channel % 8);
                output_byte_matrices[output_byte] |=
                    u64::from(lane_samples) << (channel_in_byte * 8);
            }

            for output_byte in 0..unitsize {
                let samples = transpose_8x8_bits(output_byte_matrices[output_byte]).to_le_bytes();
                for (sample, value) in samples.into_iter().enumerate() {
                    output[output_offset + sample * unitsize + output_byte] = value;
                }
            }
            output_offset += 8 * unitsize;
        }
    }
    Ok(output)
}

#[inline]
fn transpose_8x8_bits(mut rows: u64) -> u64 {
    let mut swap = (rows ^ (rows >> 7)) & 0x00aa_00aa_00aa_00aa;
    rows ^= swap ^ (swap << 7);
    swap = (rows ^ (rows >> 14)) & 0x0000_cccc_0000_cccc;
    rows ^= swap ^ (swap << 14);
    swap = (rows ^ (rows >> 28)) & 0x0000_0000_f0f0_f0f0;
    rows ^ swap ^ (swap << 28)
}

pub fn decode_cross_data_with_map(
    channel_count: u8,
    input: &[u8],
    channel_map: Option<&[u8]>,
    reverse_bits_in_word: bool,
) -> Result<Vec<u8>> {
    let unitsize = usize::from(unitsize_for_channel_count(channel_count)?);
    let channels = usize::from(channel_count);
    if let Some(map) = channel_map {
        validate_cross_channel_map(channel_count, map)?;
    }

    let stripe_bytes = channels * 8;
    if input.len() % stripe_bytes != 0 {
        return Err(CoreError::Decode(
            "cross-lane input is not stripe-aligned".to_string(),
        ));
    }

    let mut output = Vec::with_capacity(input.len());
    let mut offset = 0;
    while offset < input.len() {
        let stripe = &input[offset..offset + stripe_bytes];
        let mut words = vec![0u64; channels];
        for (channel, word) in words.iter_mut().enumerate() {
            let start = channel * 8;
            *word = u64::from_le_bytes(stripe[start..start + 8].try_into().expect("fixed slice"));
        }

        for bit_index in 0..64 {
            let mut sample_word = 0u32;
            let source_bit = if reverse_bits_in_word {
                63 - bit_index
            } else {
                bit_index
            };
            for logical_channel in 0..channels {
                let physical_channel = channel_map
                    .and_then(|map| map.get(logical_channel))
                    .map(|value| usize::from(*value))
                    .unwrap_or(logical_channel);
                let word = words[physical_channel];
                let bit_value = ((word >> source_bit) & 1) as u32;
                sample_word |= bit_value << logical_channel;
            }
            write_sample_word(&mut output, sample_word, unitsize);
        }
        offset += stripe_bytes;
    }

    Ok(output)
}

fn validate_cross_channel_map(channel_count: u8, map: &[u8]) -> Result<()> {
    let channels = usize::from(channel_count);
    if map.len() != channels {
        return Err(CoreError::Decode(format!(
            "cross-lane channel map length {} does not match channel count {channels}",
            map.len()
        )));
    }

    let mut seen = vec![false; channels];
    for &physical in map {
        let physical = usize::from(physical);
        if physical >= channels {
            return Err(CoreError::Decode(format!(
                "cross-lane channel map contains out-of-range lane {physical}"
            )));
        }
        if seen[physical] {
            return Err(CoreError::Decode(format!(
                "cross-lane channel map contains duplicate lane {physical}"
            )));
        }
        seen[physical] = true;
    }
    Ok(())
}

pub fn generate_sample_words(settings: &CaptureSettings) -> Result<CaptureData> {
    gpio_timing_for_samplerate(settings.sample_rate_hz)?;
    let unitsize = unitsize_for_channel_count(settings.channel_count)?;
    let sample_count = sample_count_from_duration(
        settings.sample_rate_hz,
        settings.duration_ms,
        settings.decode_cross,
    )?;
    generate_sample_words_with_count(settings, sample_count, unitsize)
}

pub fn generate_sample_words_with_count(
    settings: &CaptureSettings,
    sample_count: u64,
    unitsize: u8,
) -> Result<CaptureData> {
    let enabled_channels =
        resolve_enabled_channels(settings.channel_count, &settings.enabled_channels)?;
    let mut generator = DemoSampleGenerator::new(settings, unitsize)?;
    let samples = generator.generate(sample_count);

    Ok(CaptureData {
        metadata: CaptureMetadata {
            version: 1,
            source_device: settings.device_id.clone(),
            sample_rate_hz: settings.sample_rate_hz,
            channel_count: settings.channel_count,
            enabled_channels,
            unitsize,
            sample_count,
            captured_at: Utc::now(),
            labels: (0..settings.channel_count)
                .map(|index| format!("D{index}"))
                .collect(),
            trigger: None,
        },
        samples,
    })
}

pub(crate) struct DemoSampleGenerator {
    settings: CaptureSettings,
    unitsize: u8,
    channels: Vec<SaleaeDemoPulseChannel>,
    signals: Vec<DemoChannelSignal>,
    enabled_channel_mask: u32,
    next_sample: u64,
}

impl DemoSampleGenerator {
    pub(crate) fn new(settings: &CaptureSettings, unitsize: u8) -> Result<Self> {
        Self::with_signals(
            settings,
            unitsize,
            legacy_demo_signals(settings.channel_count, settings.sample_rate_hz),
        )
    }

    pub(crate) fn with_analyzers(
        settings: &CaptureSettings,
        unitsize: u8,
        analyzers: &[DemoAnalyzerSettings],
    ) -> Result<Self> {
        Self::with_signals(
            settings,
            unitsize,
            demo_signals_for_analyzers(settings.channel_count, settings.sample_rate_hz, analyzers)?,
        )
    }

    fn with_signals(
        settings: &CaptureSettings,
        unitsize: u8,
        signals: Vec<DemoChannelSignal>,
    ) -> Result<Self> {
        let enabled_channel_mask =
            enabled_channel_mask(settings.channel_count, &settings.enabled_channels)?;
        Ok(Self {
            settings: settings.clone(),
            unitsize,
            channels: (0..settings.channel_count)
                .map(SaleaeDemoPulseChannel::new)
                .collect(),
            signals,
            enabled_channel_mask,
            next_sample: 0,
        })
    }

    pub(crate) fn generate(&mut self, sample_count: u64) -> Vec<u8> {
        let capacity = usize::try_from(sample_count)
            .ok()
            .and_then(|count| count.checked_mul(usize::from(self.unitsize)))
            .unwrap_or(0);
        let mut samples = Vec::with_capacity(capacity);
        let end = self.next_sample.saturating_add(sample_count);
        while self.next_sample < end {
            let sample_index = self.next_sample;
            let mut word = 0u32;
            for channel in 0..self.settings.channel_count {
                if self.enabled_channel_mask & (1u32 << channel) == 0 {
                    continue;
                }
                let channel_index = usize::from(channel);
                let high = match &mut self.signals[channel_index] {
                    DemoChannelSignal::Pulse => self.channels[channel_index].level_at(sample_index),
                    DemoChannelSignal::Uart(settings) => {
                        demo_uart_level(sample_index, self.settings.sample_rate_hz, settings)
                            .unwrap_or_else(|| self.channels[channel_index].level_at(sample_index))
                    }
                    DemoChannelSignal::I2c { settings, role } => {
                        demo_i2c_level(sample_index, self.settings.sample_rate_hz, settings, *role)
                    }
                    DemoChannelSignal::Spi { settings, role } => {
                        demo_spi_level(sample_index, self.settings.sample_rate_hz, settings, *role)
                    }
                    DemoChannelSignal::SaleaeSimulation(signal) => signal.level_at(sample_index),
                };
                if high {
                    word |= 1u32 << channel;
                }
            }
            write_sample_word(&mut samples, word, usize::from(self.unitsize));
            self.next_sample += 1;
        }
        samples
    }
}

#[derive(Debug, Clone)]
enum DemoChannelSignal {
    Pulse,
    Uart(UartDecodeSettings),
    I2c {
        settings: I2cDecodeSettings,
        role: DemoI2cRole,
    },
    Spi {
        settings: SpiDecodeSettings,
        role: DemoSpiRole,
    },
    SaleaeSimulation(SaleaeDemoSimulation),
}

#[derive(Debug, Clone)]
struct SaleaeDemoSimulation {
    initial_high: bool,
    period_samples: u64,
    transitions: Vec<u64>,
    active_period: Option<u64>,
    next_transition: usize,
    high: bool,
}

impl SaleaeDemoSimulation {
    fn new(initial_high: bool, period_samples: u64, transitions: Vec<u64>) -> Self {
        Self {
            initial_high,
            period_samples: period_samples.max(1),
            transitions,
            active_period: None,
            next_transition: 0,
            high: initial_high,
        }
    }

    fn level_at(&mut self, sample_index: u64) -> bool {
        let period = sample_index / self.period_samples;
        let sample = sample_index % self.period_samples;
        if self.active_period != Some(period) {
            self.active_period = Some(period);
            self.next_transition = 0;
            self.high = self.initial_high;
        }
        while self
            .transitions
            .get(self.next_transition)
            .is_some_and(|transition| *transition <= sample)
        {
            self.high = !self.high;
            self.next_transition += 1;
        }
        self.high
    }
}

#[derive(Debug, Clone, Copy)]
enum DemoI2cRole {
    Sda,
    Scl,
}

#[derive(Debug, Clone, Copy)]
enum DemoSpiRole {
    Mosi,
    Miso,
    Clock,
    Enable,
}

fn legacy_demo_signals(channel_count: u8, sample_rate_hz: u64) -> Vec<DemoChannelSignal> {
    legacy_demo_signals_for_analyzers(
        channel_count,
        sample_rate_hz,
        &[
            AnalyzerDecodeSettings::Uart(UartDecodeSettings::default()),
            AnalyzerDecodeSettings::I2c(I2cDecodeSettings::default()),
            AnalyzerDecodeSettings::Spi(SpiDecodeSettings::default()),
        ],
    )
}

fn demo_signals_for_analyzers(
    channel_count: u8,
    sample_rate_hz: u64,
    analyzers: &[DemoAnalyzerSettings],
) -> Result<Vec<DemoChannelSignal>> {
    let saleae_decoder = analyzers
        .iter()
        .any(|analyzer| analyzer.backend == DecoderBackendKind::SaleaeNative)
        .then(crate::decoder_backend::SaleaeNativeDecoder::from_env)
        .transpose()
        .map_err(|error| {
            CoreError::Decode(format!(
                "Saleae Native demo generator is unavailable: {error}"
            ))
        })?;
    let mut signals = vec![DemoChannelSignal::Pulse; usize::from(channel_count)];

    for analyzer in analyzers {
        if analyzer.backend == DecoderBackendKind::SaleaeNative {
            let protocol = demo_protocol_name(&analyzer.settings);
            let simulation_samples = sample_rate_hz.clamp(100_000, 10_000_000);
            let channels = saleae_decoder
                .as_ref()
                .expect("Saleae decoder is initialized when a Saleae analyzer is present")
                .simulate(&analyzer.settings, sample_rate_hz, simulation_samples)
                .map_err(|error| {
                    CoreError::Decode(format!(
                        "Saleae Native demo generator failed for {protocol}: {error}"
                    ))
                })?;
            if channels.is_empty() {
                return Err(CoreError::Decode(format!(
                    "Saleae Native analyzer {protocol} does not provide GenerateSimulationData"
                )));
            }
            for channel in channels {
                replace_demo_channel(
                    &mut signals,
                    channel_count,
                    channel.channel,
                    DemoChannelSignal::SaleaeSimulation(SaleaeDemoSimulation::new(
                        channel.initial_high,
                        channel.sample_count,
                        channel.transitions,
                    )),
                    protocol,
                )?;
            }
            continue;
        }

        let generated = legacy_demo_signals_for_analyzers(
            channel_count,
            sample_rate_hz,
            &[analyzer.settings.clone()],
        );
        for channel in demo_channels_for_settings(&analyzer.settings) {
            replace_demo_channel(
                &mut signals,
                channel_count,
                channel,
                generated[usize::from(channel)].clone(),
                demo_protocol_name(&analyzer.settings),
            )?;
        }
    }

    Ok(signals)
}

fn demo_channels_for_settings(settings: &AnalyzerDecodeSettings) -> Vec<u8> {
    let mut channels = match settings {
        AnalyzerDecodeSettings::Uart(settings) => vec![settings.channel],
        AnalyzerDecodeSettings::I2c(settings) => vec![settings.sda_channel, settings.scl_channel],
        AnalyzerDecodeSettings::Spi(settings) => [
            settings.mosi_channel,
            settings.miso_channel,
            Some(settings.clock_channel),
            settings.enable_channel,
        ]
        .into_iter()
        .flatten()
        .collect(),
        AnalyzerDecodeSettings::Native(settings) => {
            settings.channels.values().flatten().copied().collect()
        }
    };
    channels.sort_unstable();
    channels.dedup();
    channels
}

fn legacy_demo_signals_for_analyzers(
    channel_count: u8,
    _sample_rate_hz: u64,
    analyzers: &[AnalyzerDecodeSettings],
) -> Vec<DemoChannelSignal> {
    let mut signals = vec![DemoChannelSignal::Pulse; usize::from(channel_count)];
    let mut claimed = vec![false; usize::from(channel_count)];

    for analyzer in analyzers {
        let roles = match analyzer {
            AnalyzerDecodeSettings::Uart(settings) => {
                let channels = [settings.channel];
                if !can_claim_demo_channels(&channels, &claimed) {
                    continue;
                }
                claim_demo_channel(
                    &mut signals,
                    &mut claimed,
                    settings.channel,
                    DemoChannelSignal::Uart(settings.clone()),
                );
                continue;
            }
            AnalyzerDecodeSettings::I2c(settings) => {
                let channels = [settings.sda_channel, settings.scl_channel];
                if settings.sda_channel == settings.scl_channel
                    || !can_claim_demo_channels(&channels, &claimed)
                {
                    continue;
                }
                vec![
                    (
                        settings.sda_channel,
                        DemoChannelSignal::I2c {
                            settings: settings.clone(),
                            role: DemoI2cRole::Sda,
                        },
                    ),
                    (
                        settings.scl_channel,
                        DemoChannelSignal::I2c {
                            settings: settings.clone(),
                            role: DemoI2cRole::Scl,
                        },
                    ),
                ]
            }
            AnalyzerDecodeSettings::Spi(settings) => {
                let channels = [
                    settings.mosi_channel,
                    settings.miso_channel,
                    Some(settings.clock_channel),
                    settings.enable_channel,
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
                if channels.len() < 2
                    || channels
                        .iter()
                        .copied()
                        .collect::<std::collections::HashSet<_>>()
                        .len()
                        != channels.len()
                    || !can_claim_demo_channels(&channels, &claimed)
                {
                    continue;
                }
                let mut roles = Vec::new();
                if let Some(channel) = settings.mosi_channel {
                    roles.push((
                        channel,
                        DemoChannelSignal::Spi {
                            settings: settings.clone(),
                            role: DemoSpiRole::Mosi,
                        },
                    ));
                }
                if let Some(channel) = settings.miso_channel {
                    roles.push((
                        channel,
                        DemoChannelSignal::Spi {
                            settings: settings.clone(),
                            role: DemoSpiRole::Miso,
                        },
                    ));
                }
                roles.push((
                    settings.clock_channel,
                    DemoChannelSignal::Spi {
                        settings: settings.clone(),
                        role: DemoSpiRole::Clock,
                    },
                ));
                if let Some(channel) = settings.enable_channel {
                    roles.push((
                        channel,
                        DemoChannelSignal::Spi {
                            settings: settings.clone(),
                            role: DemoSpiRole::Enable,
                        },
                    ));
                }
                roles
            }
            AnalyzerDecodeSettings::Native(_) => continue,
        };

        for (channel, signal) in roles {
            claim_demo_channel(&mut signals, &mut claimed, channel, signal);
        }
    }
    signals
}

fn demo_protocol_name(settings: &AnalyzerDecodeSettings) -> &str {
    match settings {
        AnalyzerDecodeSettings::Uart(_) => "UART",
        AnalyzerDecodeSettings::I2c(_) => "I2C",
        AnalyzerDecodeSettings::Spi(_) => "SPI",
        AnalyzerDecodeSettings::Native(settings) => &settings.protocol_name,
    }
}

fn replace_demo_channel(
    signals: &mut [DemoChannelSignal],
    channel_count: u8,
    channel: u8,
    signal: DemoChannelSignal,
    protocol: &str,
) -> Result<()> {
    if channel >= channel_count {
        return Err(CoreError::Decode(format!(
            "{protocol} Demo references D{channel}, but this capture exposes D0..D{}",
            channel_count.saturating_sub(1)
        )));
    }
    if let Some(target) = signals.get_mut(usize::from(channel)) {
        *target = signal;
    }
    Ok(())
}

fn can_claim_demo_channels(channels: &[u8], claimed: &[bool]) -> bool {
    channels.iter().all(|channel| {
        claimed
            .get(usize::from(*channel))
            .is_some_and(|claimed| !claimed)
    })
}

fn claim_demo_channel(
    signals: &mut [DemoChannelSignal],
    claimed: &mut [bool],
    channel: u8,
    signal: DemoChannelSignal,
) {
    let index = usize::from(channel);
    if let (Some(target), Some(claimed)) = (signals.get_mut(index), claimed.get_mut(index)) {
        *target = signal;
        *claimed = true;
    }
}

// Logic Pro 16 demo captures use independent runs of 1..32768 samples per channel.
// Keep the generator deterministic so screenshots, measurements, and tests are repeatable.
struct SaleaeDemoPulseChannel {
    high: bool,
    next_edge: u64,
    random: u32,
}

impl SaleaeDemoPulseChannel {
    fn new(channel: u8) -> Self {
        let mut result = Self {
            high: false,
            next_edge: 0,
            random: 0x6D2B_79F5 ^ u32::from(channel).wrapping_mul(0x9E37_79B9),
        };
        result.next_edge = result.next_run_samples();
        result
    }

    fn level_at(&mut self, sample_index: u64) -> bool {
        if sample_index >= self.next_edge {
            self.high = !self.high;
            self.next_edge = self.next_edge.saturating_add(self.next_run_samples());
        }
        self.high
    }

    fn next_run_samples(&mut self) -> u64 {
        self.random = self
            .random
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        1 + u64::from((self.random >> 17) & 0x7FFF)
    }
}

fn demo_uart_level(
    sample_index: u64,
    sample_rate_hz: u64,
    settings: &UartDecodeSettings,
) -> Option<bool> {
    const MESSAGE: &[u8] = b"PXLogic demo\n";
    const LEAD_IDLE_BITS: u64 = 4;
    const TRAIL_IDLE_BITS: u64 = 8;

    let bit_period = sample_rate_hz / u64::from(settings.baud_rate.max(1));
    if bit_period < 8 {
        return None;
    }

    let bit_index = sample_index / bit_period;
    let cycle_bits = LEAD_IDLE_BITS + MESSAGE.len() as u64 * 10 + TRAIL_IDLE_BITS;
    let cycle_bit = bit_index % cycle_bits;
    if cycle_bit < LEAD_IDLE_BITS {
        return Some(true);
    }
    let message_bit = cycle_bit - LEAD_IDLE_BITS;
    if message_bit >= MESSAGE.len() as u64 * 10 {
        return Some(true);
    }

    let frame_index = message_bit / 10;
    let frame_bit = message_bit % 10;
    let byte = MESSAGE.get(frame_index as usize)?;
    let level = match frame_bit {
        0 => Some(false),
        1..=8 => Some((byte >> (frame_bit - 1)) & 1 != 0),
        9 => Some(true),
        _ => Some(true),
    }?;
    Some(level != settings.inverted)
}

fn demo_i2c_level(
    sample_index: u64,
    sample_rate_hz: u64,
    _settings: &I2cDecodeSettings,
    role: DemoI2cRole,
) -> bool {
    let half_period = (sample_rate_hz / 200_000).max(2);
    let phase = (sample_index / half_period) % 60;
    if phase < 2 {
        return true;
    }
    if phase == 2 {
        return matches!(role, DemoI2cRole::Scl);
    }
    let data_phase = phase - 3;
    if data_phase < 54 {
        let byte_index = data_phase / 18;
        let bit_index = (data_phase % 18) / 2;
        let byte = [0x40u8, 0x90, 0x90][byte_index as usize];
        if matches!(role, DemoI2cRole::Scl) {
            return data_phase % 2 == 1;
        }
        if bit_index == 8 {
            return byte_index == 2;
        }
        return byte & (1 << (7 - bit_index)) != 0;
    }
    match (phase, role) {
        (57, _) => false,
        (58, DemoI2cRole::Scl) => true,
        (58, _) => false,
        _ => true,
    }
}

fn demo_spi_level(
    sample_index: u64,
    sample_rate_hz: u64,
    settings: &SpiDecodeSettings,
    role: DemoSpiRole,
) -> bool {
    let half_period = (sample_rate_hz / 2_000_000).max(2);
    let transfer_half_periods = u64::from(settings.bits_per_transfer.max(1)) * 2 + 4;
    let phase = (sample_index / half_period) % transfer_half_periods;
    let active = (2..transfer_half_periods - 2).contains(&phase);
    if matches!(role, DemoSpiRole::Enable) {
        return active != settings.enable_active_low;
    }
    if matches!(role, DemoSpiRole::Clock) {
        let clock = if active { phase % 2 == 1 } else { false };
        return clock != settings.clock_polarity;
    }
    if !active {
        return false;
    }
    let wire_bit = ((phase - 2) / 2).min(u64::from(settings.bits_per_transfer - 1));
    let bit_index = if settings.msb_first {
        u64::from(settings.bits_per_transfer - 1) - wire_bit
    } else {
        wire_bit
    };
    let word_index = (sample_index / (half_period * transfer_half_periods)) as u64;
    let mask = if settings.bits_per_transfer >= 64 {
        u64::MAX
    } else {
        (1u64 << settings.bits_per_transfer) - 1
    };
    let mosi = (0x4c + word_index) & mask;
    let value = if matches!(role, DemoSpiRole::Mosi) {
        mosi
    } else {
        (mosi + 1) & mask
    };
    value & (1u64 << bit_index) != 0
}

pub fn read_sample_word(samples: &[u8], unitsize: u8, sample_index: u64) -> Option<u32> {
    let stride = usize::from(unitsize);
    let offset = usize::try_from(sample_index).ok()?.checked_mul(stride)?;
    let bytes = samples.get(offset..offset + stride)?;
    let value = match unitsize {
        1 => u32::from(bytes[0]),
        2 => u32::from(u16::from_le_bytes(bytes.try_into().ok()?)),
        4 => u32::from_le_bytes(bytes.try_into().ok()?),
        _ => return None,
    };
    Some(value)
}

fn write_sample_word(output: &mut Vec<u8>, value: u32, unitsize: usize) {
    match unitsize {
        1 => output.push(value as u8),
        2 => output.extend_from_slice(&(value as u16).to_le_bytes()),
        4 => output.extend_from_slice(&value.to_le_bytes()),
        _ => unreachable!("unitsize is validated by caller"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_samplerates_to_gpio_timing() {
        assert_eq!(
            gpio_timing_for_samplerate(250_000_000).unwrap(),
            GpioTiming { mode: 2, div: 0 }
        );
        assert_eq!(
            gpio_timing_for_samplerate(10_000_000).unwrap(),
            GpioTiming { mode: 7, div: 9 }
        );
        assert_eq!(
            gpio_timing_for_samplerate(6_250_000).unwrap(),
            GpioTiming { mode: 7, div: 15 }
        );
        assert!(supported_samplerates().contains(&6_250_000));
        assert!(gpio_timing_for_samplerate(24_000_000).is_err());
    }

    #[test]
    fn builds_pxview_pwm0_disable_program_restore_sequence() {
        let settings = PwmSettings::default();
        let (configuration, script) = build_pxview_pwm0_register_script(&settings).unwrap();

        assert_eq!(configuration.period_ticks, 125_000);
        assert_eq!(configuration.high_ticks, 62_500);
        assert_eq!(configuration.effective_frequency_hz, 1_000.0);
        assert_eq!(configuration.effective_duty_percent, 50.0);
        assert_eq!(
            script,
            vec![
                RegisterWrite {
                    addr: protocol::REG_PWM0_ENABLE,
                    value: 0,
                },
                RegisterWrite {
                    addr: protocol::REG_PWM0_PERIOD,
                    value: 124_999,
                },
                RegisterWrite {
                    addr: protocol::REG_PWM0_HIGH,
                    value: 62_499,
                },
                RegisterWrite {
                    addr: protocol::REG_PWM0_ENABLE,
                    value: 0,
                },
            ]
        );

        let enabled = PwmSettings {
            enabled: true,
            frequency_hz: 1_000_000.0,
            duty_percent: 0.0,
        };
        let (configuration, script) = build_pxview_pwm0_register_script(&enabled).unwrap();
        assert_eq!(configuration.period_ticks, 125);
        assert_eq!(configuration.high_ticks, 0);
        assert_eq!(script[2].value, u32::MAX);
        assert_eq!(script[3].value, 1);
    }

    #[test]
    fn validates_pxview_pwm0_public_control_range() {
        for frequency_hz in [0.0, -1.0, 1_000_001.0, f64::NAN] {
            assert!(build_pxview_pwm0_register_script(&PwmSettings {
                frequency_hz,
                ..PwmSettings::default()
            })
            .is_err());
        }
        for duty_percent in [-1.0, 100.1, f64::INFINITY] {
            assert!(build_pxview_pwm0_register_script(&PwmSettings {
                duty_percent,
                ..PwmSettings::default()
            })
            .is_err());
        }
    }

    #[test]
    fn builds_pxlogic_capture_script() {
        let script =
            build_capture_register_script(4096, 65_536, 16, 250_000_000, CaptureProfile::default())
                .unwrap();
        assert_eq!(script.len(), 27);
        assert_eq!(
            script[0],
            RegisterWrite {
                addr: protocol::REG_THRESHOLD_PWM_MAX,
                value: 12_000
            }
        );
        assert_eq!(
            script[11],
            RegisterWrite {
                addr: protocol::REG_CAPTURE_BYTES_LOW,
                value: 69_632
            }
        );
        assert_eq!(
            script[20],
            RegisterWrite {
                addr: protocol::REG_STREAM_CHANNEL_ENABLE,
                value: 0x0000_FFFF
            }
        );
        assert!(script.contains(&RegisterWrite {
            addr: protocol::REG_PWM0_ENABLE,
            value: 0,
        }));
        assert!(script.contains(&RegisterWrite {
            addr: protocol::REG_PWM1_ENABLE,
            value: 0,
        }));
    }

    #[test]
    fn builds_sparse_pxlogic_capture_script_from_physical_channel_mask() {
        let script = build_capture_register_script_for_channels(
            4096,
            16,
            16,
            &[0, 4],
            25_000_000,
            CaptureProfile::default(),
        )
        .unwrap();
        assert!(script.contains(&RegisterWrite {
            addr: protocol::REG_CAPTURE_CHANNEL_COUNT,
            value: 2,
        }));
        assert!(script.contains(&RegisterWrite {
            addr: protocol::REG_STREAM_CHANNEL_ENABLE,
            value: 0x0000_0011,
        }));
    }

    #[test]
    fn writes_pxview_equivalent_threshold_voltage() {
        let profile = CaptureProfile {
            vth_volts: 3.3,
            ..CaptureProfile::default()
        };
        let script =
            build_capture_register_script_for_channels(4096, 16, 1, &[0], 10_000_000, profile)
                .unwrap();

        let threshold = script
            .iter()
            .find(|write| write.addr == protocol::REG_THRESHOLD_VALUE)
            .expect("threshold register");
        // PXView: vth * (100 / 200) / 3.334 * (120 MHz / 10 kHz).
        assert_eq!(threshold.value, (3.3 * 0.5 / 3.334 * 12_000.0) as u32);
    }

    #[test]
    fn rejects_thresholds_outside_the_hardware_pwm_range() {
        let settings = CaptureSettings {
            threshold_volts: 6.669,
            ..CaptureSettings::default()
        };
        assert!(capture_profile_from_settings(&settings, 8).is_err());

        let settings = CaptureSettings {
            threshold_volts: 6.668,
            ..CaptureSettings::default()
        };
        assert!(capture_profile_from_settings(&settings, 8).is_ok());
    }

    #[test]
    fn validates_and_canonicalizes_enabled_physical_channels() {
        assert_eq!(
            resolve_enabled_channels(8, &[]).unwrap(),
            (0..8).collect::<Vec<_>>()
        );
        assert_eq!(resolve_enabled_channels(8, &[4, 0]).unwrap(), vec![0, 4]);
        assert!(resolve_enabled_channels(8, &[0, 0]).is_err());
        assert!(resolve_enabled_channels(8, &[0, 8]).is_err());
        assert_eq!(enabled_channel_mask(8, &[0, 4]).unwrap(), 0x11);
    }

    #[test]
    fn maps_capture_settings_to_pxview_trigger_and_filter_profile() {
        let profile = capture_profile_from_settings(
            &CaptureSettings {
                trigger_enabled: true,
                trigger_channel: 3,
                trigger_kind: CaptureTriggerKind::Falling,
                glitch_filter_enabled: true,
                external_trigger_mode: crate::ExternalTriggerMode::Edge,
                clock_edge: true,
                trigger_out_enabled: true,
                mode: CaptureMode::Buffer,
                threshold_volts: 1.8,
                ..CaptureSettings::default()
            },
            16,
        )
        .unwrap();

        assert_eq!(profile.filter, 1);
        assert_eq!(profile.trigger_fall, 1 << 3);
        assert_eq!(profile.trigger_rise, 0);
        assert_eq!(profile.trigger_one, 0);
        assert_eq!(profile.trigger_zero, 0);
        assert_eq!(profile.ext_trigger_mode, 5);
        assert_eq!(profile.clock_edge, 1);
        assert_eq!(profile.trigger_out_enable, 1);

        let script = build_capture_register_script(4096, 65_536, 16, 250_000_000, profile).unwrap();
        assert!(script.contains(&RegisterWrite {
            addr: protocol::REG_TRIGGER_FALL,
            value: 1 << 3,
        }));
        assert!(script.contains(&RegisterWrite {
            addr: protocol::REG_EXT_TRIGGER_MODE,
            value: 5,
        }));
        assert!(script.contains(&RegisterWrite {
            addr: protocol::REG_TRIGGER_OUT_ENABLE,
            value: 1,
        }));
        assert!(script.contains(&RegisterWrite {
            addr: protocol::REG_GPIO_MODE,
            value: 2 | (1 << protocol::STREAM_FILTER_SHIFT),
        }));

        let disabled_trigger = CaptureSettings {
            channel_count: 8,
            enabled_channels: vec![0, 4],
            trigger_enabled: true,
            trigger_channel: 3,
            ..CaptureSettings::default()
        };
        assert!(capture_profile_from_settings(&disabled_trigger, 8).is_err());
        assert!(script.contains(&RegisterWrite {
            addr: protocol::REG_STREAM_CONTROL,
            value: 1 << protocol::STREAM_FILTER_SHIFT,
        }));
    }

    #[test]
    fn maps_additional_level_conditions_to_pxview_trigger_masks() {
        let profile = capture_profile_from_settings(
            &CaptureSettings {
                channel_count: 8,
                enabled_channels: vec![0, 1, 2, 3],
                trigger_enabled: true,
                trigger_channel: 0,
                trigger_kind: CaptureTriggerKind::Rising,
                trigger_high_mask: 1 << 2,
                trigger_low_mask: 1 << 3,
                ..CaptureSettings::default()
            },
            8,
        )
        .unwrap();

        assert_eq!(profile.trigger_rise, 1);
        assert_eq!(profile.trigger_one, 1 << 2);
        assert_eq!(profile.trigger_zero, 1 << 3);
    }

    #[test]
    fn computes_pxview_trigger_position_and_hardware_depth_clamps() {
        let stream = CaptureSettings {
            mode: CaptureMode::Stream,
            trigger_position_percent: 50,
            ..CaptureSettings::default()
        };
        assert_eq!(
            pxview_trigger_position(&stream, 20_000_000, 32, 4_000_000_000).unwrap(),
            10_000_000
        );
        let channel_depth = (4_000_000_000u64 / 32) & !1023;
        assert_eq!(
            pxview_trigger_position(&stream, 100_000_000, 32, 4_000_000_000).unwrap(),
            (channel_depth / 10) as u32
        );

        let buffer = CaptureSettings {
            mode: CaptureMode::Buffer,
            ..stream.clone()
        };
        assert_eq!(
            pxview_trigger_position(&buffer, 100_000_000, 32, 4_000_000_000).unwrap(),
            50_000_000
        );

        let capture_start = CaptureSettings {
            trigger_position_percent: 0,
            ..stream.clone()
        };
        assert_eq!(
            pxview_trigger_position(&capture_start, 100_000, 32, 4_000_000_000).unwrap(),
            64
        );

        let invalid = CaptureSettings {
            trigger_position_percent: 101,
            ..stream
        };
        assert!(pxview_trigger_position(&invalid, 100_000, 32, 4_000_000_000).is_err());
    }

    #[test]
    fn computes_pxview_transfer_size() {
        assert_eq!(
            pxview_capture_transfer_size(25_000_000, 32, true).unwrap(),
            1_048_576
        );
        assert_eq!(
            pxview_capture_transfer_size(25_000_000, 32, false).unwrap(),
            524_288
        );
        assert_eq!(
            pxview_capture_transfer_size(25_000_000, 16, true).unwrap(),
            524_288
        );
        assert!(pxview_capture_transfer_size(25_000_000, 0, true).is_err());
    }

    #[test]
    fn computes_pxview_cross_raw_bytes_for_all_channel_counts() {
        assert_eq!(align_cross_sample_count(1).unwrap(), 64);
        assert_eq!(align_cross_sample_count(64).unwrap(), 64);
        assert_eq!(align_cross_sample_count(65).unwrap(), 128);

        for channel_count in 1u8..=32 {
            assert_eq!(
                pxview_cross_raw_byte_count(64, channel_count).unwrap(),
                u64::from(channel_count) * 8
            );
        }
    }

    #[test]
    fn decodes_cross_lane_16_channel_stripe() {
        let mut input = vec![0u8; 16 * 8];
        let ch0 = 0b1010u64;
        let ch1 = 0b0101u64;
        input[0..8].copy_from_slice(&ch0.to_le_bytes());
        input[8..16].copy_from_slice(&ch1.to_le_bytes());

        let decoded = decode_cross_data(16, &input).unwrap();
        assert_eq!(decoded.len(), 64 * 2);
        assert_eq!(read_sample_word(&decoded, 2, 0).unwrap() & 0b11, 0b10);
        assert_eq!(read_sample_word(&decoded, 2, 1).unwrap() & 0b11, 0b01);
        assert_eq!(read_sample_word(&decoded, 2, 2).unwrap() & 0b11, 0b10);
        assert_eq!(read_sample_word(&decoded, 2, 3).unwrap() & 0b11, 0b01);
    }

    #[test]
    fn decodes_sparse_cross_lanes_into_physical_sample_bits() {
        let mut input = Vec::new();
        input.extend_from_slice(&0b1010u64.to_le_bytes());
        input.extend_from_slice(&0b0101u64.to_le_bytes());

        let decoded = decode_cross_data_to_physical_channels(5, &[0, 4], &input, false).unwrap();
        assert_eq!(decoded.len(), 64);
        assert_eq!(read_sample_word(&decoded, 1, 0), Some(1 << 4));
        assert_eq!(read_sample_word(&decoded, 1, 1), Some(1));
        assert_eq!(read_sample_word(&decoded, 1, 2), Some(1 << 4));
        assert_eq!(read_sample_word(&decoded, 1, 3), Some(1));
        assert!(decoded.iter().all(|word| word & !0x11 == 0));
    }

    #[test]
    fn decodes_cross_lane_for_every_pxview_channel_width() {
        for channel_count in [1u8, 2, 4, 8, 16, 32] {
            let mut input = Vec::new();
            for channel in 0..channel_count {
                let word = if channel % 2 == 0 {
                    0xAAAA_AAAA_AAAA_AAAAu64
                } else {
                    0x5555_5555_5555_5555u64
                };
                input.extend_from_slice(&word.to_le_bytes());
            }

            let decoded = decode_cross_data(channel_count, &input).unwrap();
            let unitsize = unitsize_for_channel_count(channel_count).unwrap();
            let even_mask = (0..channel_count)
                .filter(|channel| channel % 2 == 0)
                .fold(0u32, |mask, channel| mask | (1u32 << channel));
            let odd_mask = (0..channel_count)
                .filter(|channel| channel % 2 == 1)
                .fold(0u32, |mask, channel| mask | (1u32 << channel));

            assert_eq!(decoded.len(), 64 * usize::from(unitsize));
            assert_eq!(read_sample_word(&decoded, unitsize, 0), Some(odd_mask));
            assert_eq!(read_sample_word(&decoded, unitsize, 1), Some(even_mask));
        }
    }

    #[test]
    fn decodes_cross_lane_with_explicit_channel_map() {
        let mut input = vec![0u8; 16 * 8];
        input[0..8].copy_from_slice(&0b0001u64.to_le_bytes());
        input[8..16].copy_from_slice(&0b0010u64.to_le_bytes());
        let mut map: Vec<u8> = (0..16).collect();
        map[0] = 1;
        map[1] = 0;

        let decoded = decode_cross_data_with_map(16, &input, Some(&map), false).unwrap();
        assert_eq!(read_sample_word(&decoded, 2, 0).unwrap() & 0b11, 0b10);
        assert_eq!(read_sample_word(&decoded, 2, 1).unwrap() & 0b11, 0b01);
    }

    #[test]
    fn decodes_cross_lane_with_reversed_time_bits() {
        let mut input = vec![0u8; 16 * 8];
        input[0..8].copy_from_slice(&(1u64 << 63).to_le_bytes());
        let map: Vec<u8> = (0..16).collect();

        let decoded = decode_cross_data_with_map(16, &input, Some(&map), true).unwrap();
        assert_eq!(read_sample_word(&decoded, 2, 0).unwrap() & 0b1, 0b1);
        assert_eq!(read_sample_word(&decoded, 2, 63).unwrap() & 0b1, 0b0);
    }

    #[test]
    fn fake_capture_has_expected_size() {
        let settings = CaptureSettings {
            sample_rate_hz: 10_000,
            duration_ms: 10,
            channel_count: 16,
            ..CaptureSettings::default()
        };
        let capture = generate_sample_words(&settings).unwrap();
        assert_eq!(capture.metadata.sample_count, 128);
        assert_eq!(
            capture.metadata.enabled_channels,
            (0..16).collect::<Vec<_>>()
        );
        assert_eq!(capture.samples.len(), 128 * 2);
    }

    #[test]
    fn demo_channels_have_varied_pulse_widths() {
        let capture = generate_sample_words(&CaptureSettings {
            sample_rate_hz: 25_000_000,
            duration_ms: 10,
            channel_count: 4,
            ..CaptureSettings::default()
        })
        .unwrap();

        let mut previous = false;
        let mut last_edge = None;
        let mut run_lengths = Vec::new();
        for sample in 0..capture.metadata.sample_count {
            let high = read_sample_word(&capture.samples, capture.metadata.unitsize, sample)
                .map(|word| word & (1 << 1) != 0)
                .unwrap();
            if high != previous {
                if let Some(last) = last_edge {
                    run_lengths.push(sample - last);
                }
                last_edge = Some(sample);
                previous = high;
            }
        }

        assert!(run_lengths.len() >= 5);
        assert!(run_lengths.windows(2).any(|pair| pair[0] != pair[1]));
        assert!(run_lengths
            .iter()
            .all(|length| (1..=32_768).contains(length)));
    }

    #[test]
    #[ignore = "requires the bundled Saleae standalone sidecar"]
    fn saleae_native_demo_uses_official_swd_waveform_and_decodes_it() {
        use std::collections::BTreeMap;

        use crate::{
            decode::NativeDecodeSettings,
            decoder_backend::{DecoderBackend, SaleaeNativeDecoder},
        };

        let analyzer = DemoAnalyzerSettings {
            backend: DecoderBackendKind::SaleaeNative,
            settings: AnalyzerDecodeSettings::Native(NativeDecodeSettings {
                decoder_id: "swd".to_string(),
                protocol_name: "SWD".to_string(),
                channels: BTreeMap::from([
                    ("SWDIO".to_string(), Some(3)),
                    ("SWCLK".to_string(), Some(4)),
                ]),
                options: BTreeMap::new(),
                primary_channel: 3,
            }),
        };
        let settings = CaptureSettings {
            device_id: "fake:pxlogic-demo".to_string(),
            sample_rate_hz: 25_000_000,
            channel_count: 8,
            enabled_channels: (0..8).collect(),
            ..CaptureSettings::default()
        };
        let sample_count = 500_000;
        let signals = demo_signals_for_analyzers(
            settings.channel_count,
            settings.sample_rate_hz,
            &[analyzer.clone()],
        )
        .unwrap();
        let DemoChannelSignal::SaleaeSimulation(clock) = &signals[4] else {
            panic!("D4 must use the official SWD simulation clock");
        };
        assert!(clock.transitions.len() > 10_000);
        assert_eq!(clock.transitions[0], 250_007);
        assert_eq!(clock.transitions[1] - clock.transitions[0], 10);
        assert_eq!(clock.transitions[2] - clock.transitions[1], 15);

        let mut generator = DemoSampleGenerator::with_signals(&settings, 1, signals).unwrap();
        let capture = CaptureData {
            metadata: CaptureMetadata {
                version: 1,
                source_device: settings.device_id.clone(),
                sample_rate_hz: settings.sample_rate_hz,
                channel_count: settings.channel_count,
                enabled_channels: settings.enabled_channels.clone(),
                unitsize: 1,
                sample_count,
                captured_at: Utc::now(),
                labels: (0..settings.channel_count)
                    .map(|channel| format!("D{channel}"))
                    .collect(),
                trigger: None,
            },
            samples: generator.generate(sample_count),
        };
        let output = SaleaeNativeDecoder::from_env()
            .unwrap()
            .decode(&capture, &analyzer.settings)
            .unwrap();
        assert!(!output.frames.is_empty());
        assert!(output
            .frames
            .iter()
            .any(|frame| !frame.channel_values.is_empty()));
    }
}
