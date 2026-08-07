use std::collections::BTreeMap;

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    capture::read_sample_word,
    error::{CoreError, Result},
    models::CaptureData,
};

const MAX_DECODED_FRAMES: usize = 100_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UartParity {
    None,
    Even,
    Odd,
}

impl Default for UartParity {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UartStopBits {
    One,
    OnePointFive,
    Two,
}

impl UartStopBits {
    pub const fn as_f64(self) -> f64 {
        match self {
            Self::One => 1.0,
            Self::OnePointFive => 1.5,
            Self::Two => 2.0,
        }
    }

    pub fn from_f64(value: f64) -> Option<Self> {
        if (value - 1.0).abs() < f64::EPSILON {
            Some(Self::One)
        } else if (value - 1.5).abs() < f64::EPSILON {
            Some(Self::OnePointFive)
        } else if (value - 2.0).abs() < f64::EPSILON {
            Some(Self::Two)
        } else {
            None
        }
    }
}

impl Default for UartStopBits {
    fn default() -> Self {
        Self::One
    }
}

impl Serialize for UartStopBits {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(self.as_f64())
    }
}

impl<'de> Deserialize<'de> for UartStopBits {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Self::from_f64(value).ok_or_else(|| D::Error::custom("UART stop bits must be 1, 1.5, or 2"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UartDecodeSettings {
    pub channel: u8,
    pub baud_rate: u32,
    pub inverted: bool,
    #[serde(default = "default_uart_data_bits")]
    pub data_bits: u8,
    #[serde(default = "default_uart_stop_bits")]
    pub stop_bits: UartStopBits,
    #[serde(default)]
    pub parity: UartParity,
    #[serde(default)]
    pub msb_first: bool,
}

const fn default_uart_data_bits() -> u8 {
    8
}

const fn default_uart_stop_bits() -> UartStopBits {
    UartStopBits::One
}

impl Default for UartDecodeSettings {
    fn default() -> Self {
        Self {
            channel: 0,
            baud_rate: 115_200,
            inverted: false,
            data_bits: default_uart_data_bits(),
            stop_bits: default_uart_stop_bits(),
            parity: UartParity::None,
            msb_first: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct I2cDecodeSettings {
    pub sda_channel: u8,
    pub scl_channel: u8,
}

impl Default for I2cDecodeSettings {
    fn default() -> Self {
        Self {
            sda_channel: 1,
            scl_channel: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpiDecodeSettings {
    pub mosi_channel: Option<u8>,
    pub miso_channel: Option<u8>,
    pub clock_channel: u8,
    pub enable_channel: Option<u8>,
    pub enable_active_low: bool,
    pub clock_polarity: bool,
    pub clock_phase: bool,
    pub bits_per_transfer: u8,
    pub msb_first: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum NativeOptionValue {
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Text(String),
}

// Values arrive through JSON, which cannot represent NaN; equality is therefore total here.
impl Eq for NativeOptionValue {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeDecodeSettings {
    pub decoder_id: String,
    pub protocol_name: String,
    pub channels: BTreeMap<String, Option<u8>>,
    pub options: BTreeMap<String, NativeOptionValue>,
    pub primary_channel: u8,
}

impl Default for SpiDecodeSettings {
    fn default() -> Self {
        Self {
            mosi_channel: Some(3),
            miso_channel: Some(4),
            clock_channel: 5,
            enable_channel: Some(6),
            enable_active_low: true,
            clock_polarity: false,
            clock_phase: false,
            bits_per_transfer: 8,
            msb_first: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "protocol", content = "settings")]
pub enum AnalyzerDecodeSettings {
    Uart(UartDecodeSettings),
    I2c(I2cDecodeSettings),
    Spi(SpiDecodeSettings),
    Native(NativeDecodeSettings),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecodedChannelValue {
    pub channel: u8,
    pub role: String,
    pub label: String,
    #[serde(default)]
    pub texts: Vec<String>,
    pub value: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecodedProtocolMarker {
    pub channel: Option<u8>,
    pub sample: u64,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecodedFrame {
    pub frame_id: u64,
    pub start_sample: u64,
    pub end_sample: u64,
    pub frame_type: String,
    pub label: String,
    pub value: u64,
    pub channel_values: Vec<DecodedChannelValue>,
    #[serde(default)]
    pub protocol_markers: Vec<DecodedProtocolMarker>,
}

struct FrameCollector {
    frames: Vec<DecodedFrame>,
    last_frame: Option<DecodedFrame>,
    seen: u64,
    stride: u64,
}

impl Default for FrameCollector {
    fn default() -> Self {
        Self {
            frames: Vec::new(),
            last_frame: None,
            seen: 0,
            stride: 1,
        }
    }
}

impl FrameCollector {
    fn push(&mut self, mut frame: DecodedFrame) {
        frame.frame_id = self.seen;
        self.seen = self.seen.saturating_add(1);
        self.last_frame = Some(frame.clone());
        if frame.frame_id % self.stride != 0 {
            return;
        }
        self.frames.push(frame);
        if self.frames.len() > MAX_DECODED_FRAMES {
            self.frames = self
                .frames
                .drain(..)
                .enumerate()
                .filter_map(|(index, frame)| (index % 2 == 0).then_some(frame))
                .collect();
            self.stride = self.stride.saturating_mul(2);
        }
    }

    fn finish(mut self) -> Vec<DecodedFrame> {
        if let Some(last) = self.last_frame {
            if self.frames.last().map(|frame| frame.frame_id) != Some(last.frame_id) {
                if self.frames.len() >= MAX_DECODED_FRAMES {
                    self.frames.pop();
                }
                self.frames.push(last);
            }
        }
        self.frames
    }
}

pub fn decode_analyzer(
    capture: &CaptureData,
    settings: &AnalyzerDecodeSettings,
) -> Result<Vec<DecodedFrame>> {
    match settings {
        AnalyzerDecodeSettings::Uart(settings) => decode_uart(capture, settings),
        AnalyzerDecodeSettings::I2c(settings) => decode_i2c(capture, settings),
        AnalyzerDecodeSettings::Spi(settings) => decode_spi(capture, settings),
        AnalyzerDecodeSettings::Native(settings) => Err(CoreError::Decode(format!(
            "{} requires a Saleae Native or Sigrok Native decoder backend",
            settings.protocol_name
        ))),
    }
}

pub fn decode_uart(
    capture: &CaptureData,
    settings: &UartDecodeSettings,
) -> Result<Vec<DecodedFrame>> {
    validate_channel(capture, settings.channel)?;
    if settings.baud_rate == 0 {
        return Err(CoreError::Decode(
            "UART baud rate must be non-zero".to_string(),
        ));
    }

    let bit_period = capture.metadata.sample_rate_hz as f64 / f64::from(settings.baud_rate);
    if bit_period < 2.0 {
        return Err(CoreError::Decode(
            "sample rate is too low for the selected UART baud rate".to_string(),
        ));
    }

    let mut frames = FrameCollector::default();
    let mut sample = 1u64;
    while sample + ((10.0 * bit_period).ceil() as u64) < capture.metadata.sample_count {
        let previous = logical_bit(capture, settings.channel, sample - 1)? ^ settings.inverted;
        let current = logical_bit(capture, settings.channel, sample)? ^ settings.inverted;
        if previous
            && !current
            && !(logical_bit_at_offset(capture, settings.channel, sample, bit_period * 0.5)?
                ^ settings.inverted)
        {
            let mut value = 0u8;
            let mut valid = true;
            for bit_index in 0..8 {
                let bit_sample =
                    sample + ((bit_period * (1.5 + f64::from(bit_index))).round() as u64);
                if bit_sample >= capture.metadata.sample_count {
                    valid = false;
                    break;
                }
                if logical_bit(capture, settings.channel, bit_sample)? ^ settings.inverted {
                    value |= 1u8 << bit_index;
                }
            }

            let stop_sample = sample + ((bit_period * 9.5).round() as u64);
            if valid
                && stop_sample < capture.metadata.sample_count
                && (logical_bit(capture, settings.channel, stop_sample)? ^ settings.inverted)
            {
                frames.push(DecodedFrame {
                    frame_id: 0,
                    start_sample: sample,
                    end_sample: sample + ((bit_period * 10.0).ceil() as u64),
                    frame_type: "data".to_string(),
                    label: label_for_value(u64::from(value), 8),
                    value: u64::from(value),
                    channel_values: vec![DecodedChannelValue {
                        channel: settings.channel,
                        role: "Input".to_string(),
                        label: label_for_value(u64::from(value), 8),
                        texts: vec![label_for_value(u64::from(value), 8)],
                        value: u64::from(value),
                    }],
                    protocol_markers: Vec::new(),
                });
                sample += ((bit_period * 9.0).floor() as u64).max(1);
                continue;
            }
        }
        sample += 1;
    }

    Ok(frames.finish())
}

pub fn decode_i2c(
    capture: &CaptureData,
    settings: &I2cDecodeSettings,
) -> Result<Vec<DecodedFrame>> {
    validate_channel(capture, settings.sda_channel)?;
    validate_channel(capture, settings.scl_channel)?;
    if settings.sda_channel == settings.scl_channel {
        return Err(CoreError::Decode(
            "I2C SDA and SCL must use different channels".to_string(),
        ));
    }
    if capture.metadata.sample_count < 2 {
        return Ok(Vec::new());
    }

    let mut frames = FrameCollector::default();
    let mut active = false;
    let mut byte = 0u8;
    let mut bit_count = 0u8;
    let mut byte_index = 0u64;
    let mut byte_start = 0u64;
    let mut previous_word = sample_word(capture, 0)?;

    for sample in 1..capture.metadata.sample_count {
        let word = sample_word(capture, sample)?;
        let previous_sda = channel_from_word(previous_word, settings.sda_channel);
        let previous_scl = channel_from_word(previous_word, settings.scl_channel);
        let sda = channel_from_word(word, settings.sda_channel);
        let scl = channel_from_word(word, settings.scl_channel);

        let start = previous_sda && !sda && scl;
        let stop = !previous_sda && sda && scl;
        if start {
            active = true;
            byte = 0;
            bit_count = 0;
            byte_index = 0;
            byte_start = sample;
        } else if stop && active {
            active = false;
            bit_count = 0;
        } else if active && !previous_scl && scl {
            if bit_count < 8 {
                if bit_count == 0 {
                    byte_start = sample;
                }
                byte = (byte << 1) | u8::from(sda);
                bit_count += 1;
            } else {
                let acknowledged = !sda;
                let bubble_label = if byte_index == 0 {
                    format!(
                        "Setup {} to [0x{:02X}] + {}",
                        if byte & 1 == 0 { "Write" } else { "Read" },
                        byte >> 1,
                        if acknowledged { "ACK" } else { "NAK" }
                    )
                } else {
                    format!(
                        "{} + {}",
                        label_for_value(u64::from(byte), 8),
                        if acknowledged { "ACK" } else { "NAK" }
                    )
                };
                let label = if byte_index == 0 {
                    format!(
                        "Address 0x{:02X} {} {}",
                        byte >> 1,
                        if byte & 1 == 0 { "Write" } else { "Read" },
                        if acknowledged { "ACK" } else { "NAK" }
                    )
                } else {
                    format!(
                        "Data {} {}",
                        label_for_value(u64::from(byte), 8),
                        if acknowledged { "ACK" } else { "NAK" }
                    )
                };
                frames.push(DecodedFrame {
                    frame_id: 0,
                    start_sample: byte_start,
                    end_sample: sample.saturating_add(1),
                    frame_type: if byte_index == 0 { "address" } else { "data" }.to_string(),
                    label,
                    value: u64::from(byte),
                    channel_values: vec![DecodedChannelValue {
                        channel: settings.sda_channel,
                        role: "SDA".to_string(),
                        label: bubble_label.clone(),
                        texts: vec![bubble_label],
                        value: u64::from(byte),
                    }],
                    protocol_markers: Vec::new(),
                });
                byte = 0;
                bit_count = 0;
                byte_index += 1;
            }
        }

        previous_word = word;
    }

    Ok(frames.finish())
}

pub fn decode_spi(
    capture: &CaptureData,
    settings: &SpiDecodeSettings,
) -> Result<Vec<DecodedFrame>> {
    validate_channel(capture, settings.clock_channel)?;
    if let Some(channel) = settings.mosi_channel {
        validate_channel(capture, channel)?;
    }
    if let Some(channel) = settings.miso_channel {
        validate_channel(capture, channel)?;
    }
    if let Some(channel) = settings.enable_channel {
        validate_channel(capture, channel)?;
    }
    if settings.mosi_channel.is_none() && settings.miso_channel.is_none() {
        return Err(CoreError::Decode(
            "SPI requires a MOSI or MISO channel".to_string(),
        ));
    }
    if !(1..=64).contains(&settings.bits_per_transfer) {
        return Err(CoreError::Decode(
            "SPI bits per transfer must be between 1 and 64".to_string(),
        ));
    }
    for channel in [
        settings.mosi_channel,
        settings.miso_channel,
        settings.enable_channel,
    ]
    .into_iter()
    .flatten()
    {
        if channel == settings.clock_channel {
            return Err(CoreError::Decode(
                "SPI clock must use a different channel than data and enable".to_string(),
            ));
        }
    }
    if capture.metadata.sample_count < 2 {
        return Ok(Vec::new());
    }

    let mut frames = FrameCollector::default();
    let mut previous_word = sample_word(capture, 0)?;
    let mut transfer_start = 0u64;
    let mut bit_count = 0u8;
    let mut mosi_value = 0u64;
    let mut miso_value = 0u64;
    let sample_rising_edge = settings.clock_polarity == settings.clock_phase;

    for sample in 1..capture.metadata.sample_count {
        let word = sample_word(capture, sample)?;
        let previous_clock = channel_from_word(previous_word, settings.clock_channel);
        let clock = channel_from_word(word, settings.clock_channel);
        let rising = !previous_clock && clock;
        let falling = previous_clock && !clock;
        let enabled = settings.enable_channel.map_or(true, |channel| {
            channel_from_word(word, channel) != settings.enable_active_low
        });

        if !enabled {
            bit_count = 0;
            mosi_value = 0;
            miso_value = 0;
        } else if (sample_rising_edge && rising) || (!sample_rising_edge && falling) {
            if bit_count == 0 {
                transfer_start = sample;
            }
            if let Some(channel) = settings.mosi_channel {
                mosi_value = append_spi_bit(
                    mosi_value,
                    channel_from_word(word, channel),
                    bit_count,
                    settings.bits_per_transfer,
                    settings.msb_first,
                );
            }
            if let Some(channel) = settings.miso_channel {
                miso_value = append_spi_bit(
                    miso_value,
                    channel_from_word(word, channel),
                    bit_count,
                    settings.bits_per_transfer,
                    settings.msb_first,
                );
            }
            bit_count += 1;

            if bit_count == settings.bits_per_transfer {
                let mut channel_values = Vec::new();
                if let Some(channel) = settings.mosi_channel {
                    channel_values.push(DecodedChannelValue {
                        channel,
                        role: "MOSI".to_string(),
                        label: label_for_value(mosi_value, settings.bits_per_transfer),
                        texts: vec![label_for_value(mosi_value, settings.bits_per_transfer)],
                        value: mosi_value,
                    });
                }
                if let Some(channel) = settings.miso_channel {
                    channel_values.push(DecodedChannelValue {
                        channel,
                        role: "MISO".to_string(),
                        label: label_for_value(miso_value, settings.bits_per_transfer),
                        texts: vec![label_for_value(miso_value, settings.bits_per_transfer)],
                        value: miso_value,
                    });
                }
                let label = match (settings.mosi_channel, settings.miso_channel) {
                    (Some(_), Some(_)) => format!(
                        "MOSI {} / MISO {}",
                        label_for_value(mosi_value, settings.bits_per_transfer),
                        label_for_value(miso_value, settings.bits_per_transfer)
                    ),
                    (Some(_), None) => format!(
                        "MOSI {}",
                        label_for_value(mosi_value, settings.bits_per_transfer)
                    ),
                    (None, Some(_)) => format!(
                        "MISO {}",
                        label_for_value(miso_value, settings.bits_per_transfer)
                    ),
                    (None, None) => unreachable!(),
                };
                frames.push(DecodedFrame {
                    frame_id: 0,
                    start_sample: transfer_start,
                    end_sample: sample.saturating_add(1),
                    frame_type: "result".to_string(),
                    label,
                    value: settings.mosi_channel.map_or(miso_value, |_| mosi_value),
                    channel_values,
                    protocol_markers: Vec::new(),
                });
                bit_count = 0;
                mosi_value = 0;
                miso_value = 0;
            }
        }

        previous_word = word;
    }

    Ok(frames.finish())
}

fn validate_channel(capture: &CaptureData, channel: u8) -> Result<()> {
    if channel >= capture.metadata.channel_count {
        Err(CoreError::InvalidChannelCount(channel))
    } else {
        Ok(())
    }
}

fn append_spi_bit(
    value: u64,
    bit: bool,
    bit_index: u8,
    bits_per_transfer: u8,
    msb_first: bool,
) -> u64 {
    if msb_first {
        (value << 1) | u64::from(bit)
    } else {
        value | (u64::from(bit) << u32::from(bit_index.min(bits_per_transfer - 1)))
    }
}

fn logical_bit_at_offset(
    capture: &CaptureData,
    channel: u8,
    start: u64,
    offset: f64,
) -> Result<bool> {
    logical_bit(capture, channel, start + offset.round() as u64)
}

fn logical_bit(capture: &CaptureData, channel: u8, sample: u64) -> Result<bool> {
    Ok(channel_from_word(sample_word(capture, sample)?, channel))
}

fn sample_word(capture: &CaptureData, sample: u64) -> Result<u32> {
    read_sample_word(&capture.samples, capture.metadata.unitsize, sample)
        .ok_or_else(|| CoreError::Decode("sample index is outside capture data".to_string()))
}

fn channel_from_word(word: u32, channel: u8) -> bool {
    word & (1u32 << channel) != 0
}

fn label_for_value(value: u64, bits: u8) -> String {
    let digits = usize::from(bits.div_ceil(4));
    format!("0x{value:0digits$X}")
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use crate::{capture::generate_sample_words, CaptureMetadata, CaptureSettings};

    use super::*;

    fn capture_from_words(words: &[u8], channel_count: u8) -> CaptureData {
        CaptureData {
            metadata: CaptureMetadata {
                version: 1,
                source_device: "test".to_string(),
                sample_rate_hz: 1_000_000,
                channel_count,
                enabled_channels: (0..channel_count).collect(),
                unitsize: 1,
                sample_count: words.len() as u64,
                captured_at: Utc::now(),
                labels: (0..channel_count)
                    .map(|channel| format!("D{channel}"))
                    .collect(),
                trigger: None,
            },
            samples: words.to_vec(),
        }
    }

    fn push_i2c_half(words: &mut Vec<u8>, sda: bool, scl: bool) {
        let word = u8::from(sda) | (u8::from(scl) << 1);
        words.extend(std::iter::repeat(word).take(4));
    }

    fn push_i2c_byte(words: &mut Vec<u8>, value: u8, ack: bool) {
        for bit in (0..8).rev() {
            let sda = value & (1 << bit) != 0;
            push_i2c_half(words, sda, false);
            push_i2c_half(words, sda, true);
        }
        push_i2c_half(words, !ack, false);
        push_i2c_half(words, !ack, true);
    }

    #[test]
    fn decodes_demo_uart_from_channel_zero() {
        let capture = generate_sample_words(&CaptureSettings {
            sample_rate_hz: 25_000_000,
            duration_ms: 10,
            channel_count: 16,
            ..CaptureSettings::default()
        })
        .unwrap();
        let frames = decode_uart(&capture, &UartDecodeSettings::default()).unwrap();
        let text: String = frames
            .iter()
            .map(|frame| frame.value as u8 as char)
            .collect();
        assert!(text.starts_with("PXLogic"));
    }

    #[test]
    fn decodes_demo_i2c_from_generated_capture() {
        let capture = generate_sample_words(&CaptureSettings {
            sample_rate_hz: 25_000_000,
            duration_ms: 10,
            channel_count: 16,
            ..CaptureSettings::default()
        })
        .unwrap();
        let frames = decode_i2c(
            &capture,
            &I2cDecodeSettings {
                sda_channel: 1,
                scl_channel: 2,
            },
        )
        .unwrap();
        let labels = frames
            .iter()
            .take(3)
            .map(|frame| frame.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            ["Address 0x20 Write ACK", "Data 0x90 ACK", "Data 0x90 NAK"]
        );
        assert_eq!(frames[0].channel_values.len(), 1);
        assert_eq!(frames[0].channel_values[0].channel, 1);
        assert_eq!(frames[0].channel_values[0].role, "SDA");
        assert_eq!(
            frames[0].channel_values[0].label,
            "Setup Write to [0x20] + ACK"
        );
    }

    #[test]
    fn decodes_demo_spi_from_generated_capture() {
        let capture = generate_sample_words(&CaptureSettings {
            sample_rate_hz: 25_000_000,
            duration_ms: 10,
            channel_count: 16,
            ..CaptureSettings::default()
        })
        .unwrap();
        let frames = decode_spi(
            &capture,
            &SpiDecodeSettings {
                mosi_channel: Some(3),
                miso_channel: Some(4),
                clock_channel: 5,
                enable_channel: Some(6),
                ..SpiDecodeSettings::default()
            },
        )
        .unwrap();
        assert_eq!(
            frames.first().map(|frame| frame.label.as_str()),
            Some("MOSI 0x4C / MISO 0x4D")
        );
        let first_values = &frames[0].channel_values;
        assert_eq!(first_values.len(), 2);
        assert_eq!(first_values[0].role, "MOSI");
        assert_eq!(first_values[0].label, "0x4C");
        assert_eq!(first_values[1].role, "MISO");
        assert_eq!(first_values[1].label, "0x4D");
        assert!(frames.len() > 100);
    }

    #[test]
    fn decodes_i2c_address_data_and_ack() {
        let mut words = Vec::new();
        push_i2c_half(&mut words, true, true);
        push_i2c_half(&mut words, false, true);
        push_i2c_byte(&mut words, 0xA0, true);
        push_i2c_byte(&mut words, 0x5A, true);
        push_i2c_half(&mut words, false, false);
        push_i2c_half(&mut words, false, true);
        push_i2c_half(&mut words, true, true);

        let frames = decode_i2c(
            &capture_from_words(&words, 2),
            &I2cDecodeSettings {
                sda_channel: 0,
                scl_channel: 1,
            },
        )
        .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].value, 0xA0);
        assert_eq!(frames[0].label, "Address 0x50 Write ACK");
        assert_eq!(
            frames[0].channel_values[0].label,
            "Setup Write to [0x50] + ACK"
        );
        assert_eq!(frames[1].label, "Data 0x5A ACK");
        assert_eq!(frames[1].channel_values[0].label, "0x5A + ACK");
    }

    #[test]
    fn decodes_spi_mode_zero_msb_first() {
        let mut words = vec![1 << 3; 4];
        for bit in (0..8).rev() {
            let mosi = 0xA5 & (1 << bit) != 0;
            let miso = 0x3C & (1 << bit) != 0;
            let base = u8::from(mosi) | (u8::from(miso) << 1);
            words.extend(std::iter::repeat(base).take(3));
            words.extend(std::iter::repeat(base | (1 << 2)).take(3));
        }
        words.extend(std::iter::repeat(1 << 3).take(4));

        let frames = decode_spi(
            &capture_from_words(&words, 4),
            &SpiDecodeSettings {
                mosi_channel: Some(0),
                miso_channel: Some(1),
                clock_channel: 2,
                enable_channel: Some(3),
                ..SpiDecodeSettings::default()
            },
        )
        .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].value, 0xA5);
        assert_eq!(frames[0].label, "MOSI 0xA5 / MISO 0x3C");
        assert_eq!(
            frames[0]
                .channel_values
                .iter()
                .map(|value| (value.channel, value.role.as_str(), value.label.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "MOSI", "0xA5"), (1, "MISO", "0x3C")]
        );
    }
}
