use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct FixtureFile {
    format: String,
    cases: Vec<FixtureCase>,
}

#[derive(Debug, Deserialize)]
struct FixtureCase {
    id: String,
    sample_rate_hz: u64,
    sample_count: u64,
    channel_count: u8,
    initial_levels: Vec<u8>,
    edges: Vec<FixtureEdges>,
    decoder: FixtureDecoder,
    expected_words: Vec<ExpectedWord>,
    #[serde(default)]
    allow_additional_partial_frames: bool,
}

#[derive(Debug, Deserialize)]
struct FixtureEdges {
    channel: u8,
    samples: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct FixtureDecoder {
    protocol: String,
    channels: BTreeMap<String, u8>,
    options: FixtureOptions,
}

#[derive(Debug, Deserialize)]
struct FixtureOptions {
    cpol: u8,
    cpha: u8,
    bit_order: String,
    word_size: u8,
    cs_polarity: String,
}

#[derive(Debug, Deserialize)]
struct ExpectedWord {
    mosi: u64,
    miso: u64,
}

#[test]
fn spi_fixture_contract_is_backend_neutral_and_complete() {
    let fixtures: FixtureFile = serde_json::from_str(include_str!(
        "../../../testdata/decoders/spi-conformance-v1.json"
    ))
    .expect("SPI decoder fixtures must remain valid JSON");

    assert_eq!(fixtures.format, "pxlogic.decoder-fixture.v1");
    assert!(fixtures.cases.len() >= 6);

    let mut ids = BTreeSet::new();
    let mut modes = BTreeSet::new();
    let mut has_lsb = false;
    let mut has_cs_boundary = false;

    for case in fixtures.cases {
        assert!(ids.insert(case.id.clone()), "duplicate fixture {}", case.id);
        assert!(case.sample_rate_hz > 0);
        assert_eq!(case.initial_levels.len(), usize::from(case.channel_count));
        assert_eq!(case.decoder.protocol, "spi");
        assert_eq!(case.decoder.options.word_size, 8);
        assert_eq!(case.decoder.options.cs_polarity, "active_low");
        assert_eq!(case.decoder.channels.get("mosi"), Some(&0));
        assert_eq!(case.decoder.channels.get("miso"), Some(&1));
        assert_eq!(case.decoder.channels.get("clk"), Some(&2));
        assert_eq!(case.decoder.channels.get("cs"), Some(&3));
        modes.insert((case.decoder.options.cpol, case.decoder.options.cpha));
        has_lsb |= case.decoder.options.bit_order == "lsb_first";
        has_cs_boundary |= case.allow_additional_partial_frames;

        for edge_set in case.edges {
            assert!(edge_set.channel < case.channel_count);
            assert!(
                edge_set
                    .samples
                    .windows(2)
                    .all(|samples| samples[0] < samples[1]),
                "edges must be strictly increasing in {}",
                case.id
            );
            assert!(
                edge_set
                    .samples
                    .iter()
                    .all(|sample| *sample < case.sample_count),
                "edge outside capture in {}",
                case.id
            );
        }
        assert!(!case.expected_words.is_empty());
        assert!(case
            .expected_words
            .iter()
            .all(|word| word.mosi <= 0xff && word.miso <= 0xff));
    }

    assert_eq!(modes, BTreeSet::from([(0, 0), (0, 1), (1, 0), (1, 1)]));
    assert!(has_lsb);
    assert!(has_cs_boundary);
}
