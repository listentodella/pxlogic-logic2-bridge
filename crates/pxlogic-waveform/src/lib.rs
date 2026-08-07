use pxlogic_core::{capture::read_sample_word, CaptureData};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, WaveformError>;

#[derive(Debug, Error)]
pub enum WaveformError {
    #[error("invalid waveform request")]
    InvalidRequest,
    #[error("capture contains invalid sample data")]
    InvalidCapture,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WaveformRequest {
    pub start_sample: u64,
    pub sample_count: u64,
    pub pixels: u32,
    pub channels: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DigitalRangeMeasurementRequest {
    pub channel: u8,
    pub start_sample: u64,
    pub end_sample: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DigitalRangeMeasurementStats {
    pub channel: u8,
    pub start_sample: u64,
    pub end_sample: u64,
    pub sample_rate_hz: u64,
    pub duration_samples: u64,
    pub high_samples: u64,
    pub low_samples: u64,
    pub rising_edges: u64,
    pub falling_edges: u64,
    pub edge_count: u64,
    pub high_width_count: u64,
    pub low_width_count: u64,
    pub avg_high_width_samples: Option<f64>,
    pub avg_low_width_samples: Option<f64>,
    pub cycle_count: u64,
    pub avg_period_samples: Option<f64>,
    pub min_period_samples: Option<u64>,
    pub max_period_samples: Option<u64>,
    pub period_std_dev_samples: Option<f64>,
    pub avg_frequency_hz: Option<f64>,
    pub min_frequency_hz: Option<f64>,
    pub max_frequency_hz: Option<f64>,
    pub positive_duty: Option<f64>,
    pub negative_duty: Option<f64>,
    pub first_sample_high: bool,
    pub last_sample_high: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WaveformTile {
    pub start_sample: u64,
    pub sample_count: u64,
    pub pixels: u32,
    pub samples_per_pixel: f64,
    pub channels: Vec<ChannelTile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelTile {
    pub channel: u8,
    pub label: String,
    pub bins: Vec<WaveformBin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packed_bins: Option<String>,
    pub segments: Vec<DigitalSegment>,
    pub rising_edges: Vec<u64>,
    pub falling_edges: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_transition: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_transition: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WaveformBin {
    pub x: u32,
    pub first: bool,
    pub last: bool,
    pub has_high: bool,
    pub has_low: bool,
    pub edges: u32,
    pub first_edge_offset: Option<u32>,
    pub last_edge_offset: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DigitalSegment {
    pub start_sample: u64,
    pub end_sample: u64,
    pub high: bool,
}

pub fn build_tile(capture: &CaptureData, request: &WaveformRequest) -> Result<WaveformTile> {
    if request.pixels == 0 || request.sample_count == 0 {
        return Err(WaveformError::InvalidRequest);
    }
    let metadata = &capture.metadata;
    let total_samples = metadata.sample_count;
    let start = request.start_sample.min(total_samples);
    let end = start
        .saturating_add(request.sample_count)
        .min(total_samples);
    let visible_samples = end.saturating_sub(start);
    if visible_samples == 0 {
        return Ok(WaveformTile {
            start_sample: start,
            sample_count: 0,
            pixels: request.pixels,
            samples_per_pixel: 0.0,
            channels: Vec::new(),
        });
    }

    let bins = request.pixels.min(visible_samples as u32).max(1);
    let samples_per_bin = (visible_samples as f64) / f64::from(bins);
    let requested_channels = if request.channels.is_empty() {
        (0..metadata.channel_count).collect::<Vec<_>>()
    } else {
        request.channels.clone()
    };

    let requested_channels = requested_channels
        .into_iter()
        .filter(|channel| *channel < metadata.channel_count)
        .collect::<Vec<_>>();
    let mut channel_bins = requested_channels
        .iter()
        .map(|_| Vec::with_capacity(bins as usize))
        .collect::<Vec<_>>();

    for x in 0..bins {
        let bin_start =
            start + ((f64::from(x) * samples_per_bin).floor() as u64).min(visible_samples);
        let bin_end = start
            + ((f64::from(x + 1) * samples_per_bin).ceil() as u64)
                .min(visible_samples)
                .max(1);
        summarize_bin(
            capture,
            &requested_channels,
            x,
            bin_start,
            bin_end,
            &mut channel_bins,
        )?;
    }

    let mut channels = Vec::new();
    for (index, channel) in requested_channels.iter().copied().enumerate() {
        let label = metadata
            .labels
            .get(channel as usize)
            .cloned()
            .unwrap_or_else(|| format!("D{channel}"));
        let exact = build_exact_digital_data(capture, channel, start, end, request.pixels)?;
        let (previous_transition, next_transition) =
            find_transitions_outside_range(capture, channel, start, end)?;
        channels.push(ChannelTile {
            channel,
            label,
            bins: std::mem::take(&mut channel_bins[index]),
            packed_bins: None,
            segments: exact.segments,
            rising_edges: exact.rising_edges,
            falling_edges: exact.falling_edges,
            previous_transition,
            next_transition,
        });
    }

    Ok(WaveformTile {
        start_sample: start,
        sample_count: visible_samples,
        pixels: bins,
        samples_per_pixel: samples_per_bin,
        channels,
    })
}

fn find_transitions_outside_range(
    capture: &CaptureData,
    channel: u8,
    start: u64,
    end: u64,
) -> Result<(Option<u64>, Option<u64>)> {
    let metadata = &capture.metadata;
    let mask = 1u32 << channel;
    let level_at = |sample| {
        read_sample_word(&capture.samples, metadata.unitsize, sample)
            .map(|word| word & mask != 0)
            .ok_or(WaveformError::InvalidCapture)
    };

    let previous_transition = if start > 1 {
        let mut after = level_at(start - 1)?;
        let mut found = None;
        for sample in (1..start).rev() {
            let before = level_at(sample - 1)?;
            if before != after {
                found = Some(sample);
                break;
            }
            after = before;
        }
        found
    } else {
        None
    };

    let next_transition = if end < metadata.sample_count {
        let mut before = level_at(end - 1)?;
        let mut found = None;
        for sample in end..metadata.sample_count {
            let after = level_at(sample)?;
            if before != after {
                found = Some(sample);
                break;
            }
            before = after;
        }
        found
    } else {
        None
    };

    Ok((previous_transition, next_transition))
}

pub fn measure_digital_range(
    capture: &CaptureData,
    request: &DigitalRangeMeasurementRequest,
) -> Result<DigitalRangeMeasurementStats> {
    let metadata = &capture.metadata;
    if request.channel >= metadata.channel_count {
        return Err(WaveformError::InvalidRequest);
    }

    let total_samples = metadata.sample_count;
    let start = request.start_sample.min(total_samples);
    let end = request.end_sample.min(total_samples);
    if end <= start {
        return Err(WaveformError::InvalidRequest);
    }

    let mask = 1u32 << request.channel;
    let first_word = read_sample_word(&capture.samples, metadata.unitsize, start)
        .ok_or(WaveformError::InvalidCapture)?;
    let mut previous_high = first_word & mask != 0;
    let first_sample_high = previous_high;
    let mut run_start = start;
    let mut high_samples = 0u64;
    let mut high_widths = SampleStats::default();
    let mut low_widths = SampleStats::default();
    let mut rising_periods = SampleStats::default();
    let mut falling_periods = SampleStats::default();
    let mut last_rising: Option<u64> = None;
    let mut last_falling: Option<u64> = None;
    let mut rising_edges = 0u64;
    let mut falling_edges = 0u64;

    if start > 0 {
        let before_word = read_sample_word(&capture.samples, metadata.unitsize, start - 1)
            .ok_or(WaveformError::InvalidCapture)?;
        let before_high = before_word & mask != 0;
        if before_high != previous_high {
            if previous_high {
                rising_edges = rising_edges.saturating_add(1);
                last_rising = Some(start);
            } else {
                falling_edges = falling_edges.saturating_add(1);
                last_falling = Some(start);
            }
        }
    }

    for sample in start + 1..end {
        let word = read_sample_word(&capture.samples, metadata.unitsize, sample)
            .ok_or(WaveformError::InvalidCapture)?;
        let current_high = word & mask != 0;
        if current_high == previous_high {
            continue;
        }

        let run_width = sample.saturating_sub(run_start);
        if previous_high {
            high_samples = high_samples.saturating_add(run_width);
            high_widths.push(run_width);
            falling_edges = falling_edges.saturating_add(1);
            if let Some(previous_falling) = last_falling {
                falling_periods.push(sample.saturating_sub(previous_falling));
            }
            last_falling = Some(sample);
        } else {
            low_widths.push(run_width);
            rising_edges = rising_edges.saturating_add(1);
            if let Some(previous_rising) = last_rising {
                rising_periods.push(sample.saturating_sub(previous_rising));
            }
            last_rising = Some(sample);
        }

        run_start = sample;
        previous_high = current_high;
    }

    let trailing_width = end.saturating_sub(run_start);
    if previous_high {
        high_samples = high_samples.saturating_add(trailing_width);
        high_widths.push(trailing_width);
    } else {
        low_widths.push(trailing_width);
    }

    if end < total_samples {
        let after_word = read_sample_word(&capture.samples, metadata.unitsize, end)
            .ok_or(WaveformError::InvalidCapture)?;
        let after_high = after_word & mask != 0;
        if after_high != previous_high {
            if after_high {
                rising_edges = rising_edges.saturating_add(1);
                if let Some(previous_rising) = last_rising {
                    rising_periods.push(end.saturating_sub(previous_rising));
                }
            } else {
                falling_edges = falling_edges.saturating_add(1);
                if let Some(previous_falling) = last_falling {
                    falling_periods.push(end.saturating_sub(previous_falling));
                }
            }
        }
    }

    let duration_samples = end.saturating_sub(start);
    let low_samples = duration_samples.saturating_sub(high_samples);
    let period_stats = if rising_periods.count > 0 {
        &rising_periods
    } else {
        &falling_periods
    };
    let avg_period_samples = period_stats.average();
    let avg_frequency_hz = avg_period_samples
        .filter(|period| *period > 0.0)
        .map(|period| metadata.sample_rate_hz as f64 / period);
    let min_frequency_hz = period_stats
        .max
        .filter(|period| *period > 0)
        .map(|period| metadata.sample_rate_hz as f64 / period as f64);
    let max_frequency_hz = period_stats
        .min
        .filter(|period| *period > 0)
        .map(|period| metadata.sample_rate_hz as f64 / period as f64);

    Ok(DigitalRangeMeasurementStats {
        channel: request.channel,
        start_sample: start,
        end_sample: end,
        sample_rate_hz: metadata.sample_rate_hz,
        duration_samples,
        high_samples,
        low_samples,
        rising_edges,
        falling_edges,
        edge_count: rising_edges.saturating_add(falling_edges),
        high_width_count: high_widths.count,
        low_width_count: low_widths.count,
        avg_high_width_samples: high_widths.average(),
        avg_low_width_samples: low_widths.average(),
        cycle_count: period_stats.count,
        avg_period_samples,
        min_period_samples: period_stats.min,
        max_period_samples: period_stats.max,
        period_std_dev_samples: period_stats.std_dev(),
        avg_frequency_hz,
        min_frequency_hz,
        max_frequency_hz,
        positive_duty: if duration_samples > 0 {
            Some(high_samples as f64 * 100.0 / duration_samples as f64)
        } else {
            None
        },
        negative_duty: if duration_samples > 0 {
            Some(low_samples as f64 * 100.0 / duration_samples as f64)
        } else {
            None
        },
        first_sample_high,
        last_sample_high: previous_high,
    })
}

#[derive(Default)]
struct SampleStats {
    count: u64,
    sum: f64,
    sum_squares: f64,
    min: Option<u64>,
    max: Option<u64>,
}

impl SampleStats {
    fn push(&mut self, value: u64) {
        if value == 0 {
            return;
        }
        self.count = self.count.saturating_add(1);
        self.sum += value as f64;
        self.sum_squares += (value as f64) * (value as f64);
        self.min = Some(self.min.map_or(value, |current| current.min(value)));
        self.max = Some(self.max.map_or(value, |current| current.max(value)));
    }

    fn average(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum / self.count as f64)
    }

    fn std_dev(&self) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let average = self.sum / self.count as f64;
        let variance = (self.sum_squares / self.count as f64) - average * average;
        Some(variance.max(0.0).sqrt())
    }
}

fn summarize_bin(
    capture: &CaptureData,
    channels: &[u8],
    x: u32,
    start: u64,
    end: u64,
    output: &mut [Vec<WaveformBin>],
) -> Result<()> {
    let metadata = &capture.metadata;
    if channels.is_empty() {
        return Ok(());
    }

    let mut channel_mask = 0u32;
    let mut channel_to_output = [None::<usize>; 32];
    for (index, channel) in channels.iter().copied().enumerate() {
        channel_mask |= 1u32 << channel;
        channel_to_output[channel as usize] = Some(index);
    }

    let first_word = read_sample_word(&capture.samples, metadata.unitsize, start)
        .ok_or(WaveformError::InvalidCapture)?;
    let mut previous = first_word & channel_mask;
    let first_mask = previous;
    let mut last_mask = previous;
    let mut high_mask = previous;
    let mut low_mask = !first_word & channel_mask;
    let mut edges = vec![0u32; channels.len()];
    let mut first_edge_offsets = vec![None::<u32>; channels.len()];
    let mut last_edge_offsets = vec![None::<u32>; channels.len()];

    for sample in start + 1..end {
        let word = read_sample_word(&capture.samples, metadata.unitsize, sample)
            .ok_or(WaveformError::InvalidCapture)?;
        let masked = word & channel_mask;
        high_mask |= masked;
        low_mask |= !word & channel_mask;

        let mut changed = previous ^ masked;
        while changed != 0 {
            let channel = changed.trailing_zeros() as usize;
            if let Some(index) = channel_to_output[channel] {
                edges[index] = edges[index].saturating_add(1);
                let offset = sample.saturating_sub(start).min(u64::from(u32::MAX)) as u32;
                if first_edge_offsets[index].is_none() {
                    first_edge_offsets[index] = Some(offset);
                }
                last_edge_offsets[index] = Some(offset);
            }
            changed &= changed - 1;
        }
        previous = masked;
        last_mask = masked;
    }

    for (index, channel) in channels.iter().copied().enumerate() {
        let mask = 1u32 << channel;
        output[index].push(WaveformBin {
            x,
            first: first_mask & mask != 0,
            last: last_mask & mask != 0,
            has_high: high_mask & mask != 0,
            has_low: low_mask & mask != 0,
            edges: edges[index],
            first_edge_offset: first_edge_offsets[index],
            last_edge_offset: last_edge_offsets[index],
        });
    }

    Ok(())
}

#[derive(Default)]
struct ExactDigitalData {
    segments: Vec<DigitalSegment>,
    rising_edges: Vec<u64>,
    falling_edges: Vec<u64>,
}

fn build_exact_digital_data(
    capture: &CaptureData,
    channel: u8,
    start: u64,
    end: u64,
    pixels: u32,
) -> Result<ExactDigitalData> {
    let visible_samples = end.saturating_sub(start);
    let exact_limit = u64::from(pixels).saturating_mul(8).max(4096);
    if visible_samples == 0 || visible_samples > exact_limit {
        return Ok(ExactDigitalData::default());
    }

    let metadata = &capture.metadata;
    let mask = 1u32 << channel;
    let first_word = read_sample_word(&capture.samples, metadata.unitsize, start)
        .ok_or(WaveformError::InvalidCapture)?;
    let mut previous = first_word & mask != 0;
    let mut run_start = start;
    let mut data = ExactDigitalData {
        segments: Vec::new(),
        rising_edges: Vec::new(),
        falling_edges: Vec::new(),
    };

    for sample in start + 1..end {
        let word = read_sample_word(&capture.samples, metadata.unitsize, sample)
            .ok_or(WaveformError::InvalidCapture)?;
        let current = word & mask != 0;
        if current == previous {
            continue;
        }

        data.segments.push(DigitalSegment {
            start_sample: run_start,
            end_sample: sample,
            high: previous,
        });
        if current {
            data.rising_edges.push(sample);
        } else {
            data.falling_edges.push(sample);
        }
        run_start = sample;
        previous = current;
    }

    data.segments.push(DigitalSegment {
        start_sample: run_start,
        end_sample: end,
        high: previous,
    });
    Ok(data)
}

#[cfg(test)]
mod tests {
    use pxlogic_core::{capture::generate_sample_words, CaptureSettings};

    use super::*;

    #[test]
    fn builds_visible_waveform_tile() {
        let capture = generate_sample_words(&CaptureSettings {
            sample_rate_hz: 10_000,
            duration_ms: 10,
            channel_count: 4,
            ..CaptureSettings::default()
        })
        .unwrap();
        let tile = build_tile(
            &capture,
            &WaveformRequest {
                start_sample: 0,
                sample_count: 64,
                pixels: 16,
                channels: vec![0, 1],
            },
        )
        .unwrap();
        assert_eq!(tile.channels.len(), 2);
        assert_eq!(tile.channels[0].bins.len(), 16);
        assert!(tile.samples_per_pixel >= 1.0);
    }

    #[test]
    fn clamps_request_to_capture_bounds() {
        let capture = generate_sample_words(&CaptureSettings {
            sample_rate_hz: 10_000,
            duration_ms: 10,
            channel_count: 4,
            ..CaptureSettings::default()
        })
        .unwrap();
        let tile = build_tile(
            &capture,
            &WaveformRequest {
                start_sample: capture.metadata.sample_count - 4,
                sample_count: 100,
                pixels: 20,
                channels: vec![],
            },
        )
        .unwrap();
        assert_eq!(tile.sample_count, 4);
        assert_eq!(tile.channels.len(), 4);
    }

    #[test]
    fn measures_digital_range_edges_and_duty() {
        let capture = generate_sample_words(&CaptureSettings {
            sample_rate_hz: 10_000,
            duration_ms: 100,
            channel_count: 4,
            ..CaptureSettings::default()
        })
        .unwrap();

        let stats = measure_digital_range(
            &capture,
            &DigitalRangeMeasurementRequest {
                channel: 1,
                start_sample: 0,
                end_sample: capture.metadata.sample_count.min(100_000),
            },
        )
        .unwrap();

        assert_eq!(stats.channel, 1);
        assert!(stats.duration_samples > 0);
        assert_eq!(stats.edge_count, stats.rising_edges + stats.falling_edges);
        assert_eq!(
            stats.duration_samples,
            stats.high_samples + stats.low_samples
        );
        assert!(stats.positive_duty.unwrap_or_default() >= 0.0);
        assert!(stats.negative_duty.unwrap_or_default() >= 0.0);
    }
}
