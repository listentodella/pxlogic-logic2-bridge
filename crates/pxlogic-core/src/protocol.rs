use crate::error::{CoreError, Result};

pub const PXLOGIC_WCH_VID: u16 = 0x1A86;
pub const PXLOGIC_WCH_PID: u16 = 0x5237;
pub const PXLOGIC_LEGACY_VID: u16 = 0x16C0;
pub const PXLOGIC_LEGACY_PID: u16 = 0x05DC;

pub const BULK_EP_REG_OUT: u8 = 0x01;
pub const BULK_EP_REG_IN: u8 = 0x81;
pub const BULK_EP_DATA_OUT: u8 = 0x03;
pub const BULK_EP_DATA_IN: u8 = 0x82;
pub const BULK_EP_BUFFER_DATA_IN: u8 = 0x83;

pub const CMD_WRITE_REGISTER: u32 = 0xFEFE_0000;
pub const CMD_READ_REGISTER: u32 = 0xFEFE_0001;
pub const CMD_REGISTER_ACK: u32 = 0xFEFE_FEFE;
pub const CMD_CTL_READ: u8 = 0xB0;

pub const REG_BASE: u32 = 8192;
pub const REG_STREAM_CONTROL: u32 = 0;
pub const REG_STREAM_TRANSFER_SIZE: u32 = 7 << 2;
pub const REG_STREAM_START: u32 = 8 << 2;
pub const REG_STREAM_CHANNEL_ENABLE: u32 = 4 << 2;
pub const REG_GPIO_MODE: u32 = 5 << 2;
pub const REG_GPIO_DIV: u32 = 6 << 2;
pub const REG_TRIGGER_ZERO: u32 = 9 << 2;
pub const REG_TRIGGER_ONE: u32 = 10 << 2;
pub const REG_TRIGGER_RISE: u32 = 11 << 2;
pub const REG_TRIGGER_FALL: u32 = 12 << 2;
pub const REG_EXT_TRIGGER_MODE: u32 = 15 << 2;
pub const REG_PWM0_ENABLE: u32 = 16 << 2;
pub const REG_PWM0_PERIOD: u32 = 17 << 2;
pub const REG_PWM0_HIGH: u32 = 18 << 2;
pub const REG_PWM1_ENABLE: u32 = 19 << 2;
pub const REG_TRIGGER_OUT_ENABLE: u32 = 22 << 2;
pub const REG_THRESHOLD_PWM_MAX: u32 = 2 << 1;
pub const REG_THRESHOLD_VALUE: u32 = 2 << 2;
pub const REG_STREAM_DMA_SIZE: u32 = REG_BASE + 2 * 4;
pub const REG_READ_DATA_START: u32 = REG_BASE + 3 * 4;
pub const REG_READ_DATA_END: u32 = REG_BASE + 4 * 4;
pub const REG_READ_DATA_MODE: u32 = REG_BASE + 5 * 4;
pub const REG_WRITE_DATA_START: u32 = REG_BASE + 6 * 4;
pub const REG_WRITE_DATA_END: u32 = REG_BASE + 7 * 4;
pub const REG_WRITE_DATA_MODE: u32 = REG_BASE + 8 * 4;
pub const REG_CAPTURE_BYTES_LOW: u32 = REG_BASE + 9 * 4;
pub const REG_CAPTURE_BYTES_HIGH: u32 = REG_BASE + 10 * 4;
pub const REG_DEVICE_RESET: u32 = REG_BASE + 12 * 4;
pub const REG_FIRMWARE_VERSION: u32 = REG_BASE + 13 * 4;
pub const REG_BLOCK_START: u32 = REG_BASE + 11 * 4;
pub const REG_CAPTURE_CHANNEL_COUNT: u32 = REG_BASE + 19 * 4;
pub const REG_CAPTURE_TRIGGER_POS: u32 = REG_BASE + 20 * 4;
pub const REG_LOGIC_MODE: u32 = REG_BASE + 22 * 4;

pub const STREAM_MODE_BIT: u32 = 1 << 1;
pub const STREAM_ENABLE_FLAGS_BASE: u32 = 0x0000_0005;
pub const STREAM_ENABLE_PULSE_FLAG: u32 = 1 << 4;
pub const STREAM_FILTER_SHIFT: u32 = 3;
pub const STREAM_START_FLAGS: u32 = 0x0000_0000;
pub const STREAM_STOP_FLAGS: u32 = 0xFFFF_FFFF;

pub const DEFAULT_TRANSFER_SIZE: usize = 256 * 1024;
pub const DEFAULT_REGISTER_TIMEOUT_MS: u64 = 1_000;
pub const DEFAULT_BULK_TIMEOUT_MS: u64 = 1_000;
/// PXView passes a zero timeout for the FPGA loader bulk transfer, meaning it
/// waits until the device accepts the complete page-aligned bitstream.
pub const FPGA_UPLOAD_TIMEOUT_MS: u64 = 0;
pub const DATA_MODE_FPGA_DDR: u32 = 2;
pub const DATA_READ_BASE_ADDR: u32 = 0x1;
/// Firmware version of the MCU image the Bridge ships by default. Used only as
/// a fallback: [`declared_firmware_version`] reads the version out of the image
/// that is actually going to be flashed, so a selected non-default image is
/// never compared against the wrong target.
pub const EXPECTED_FIRMWARE_VERSION: u32 = 0x5690_0028;
pub const BOOTLOADER_FIRMWARE_VERSION: u32 = 0x5690_0000;
pub const PWM_CLOCK_HZ: u32 = 125_000_000;
pub const PWM_MAX_FREQUENCY_HZ: f64 = 1_000_000.0;

/// High half of every PXView MCU firmware version, so a candidate constant can
/// be told apart from unrelated immediates in the image.
const FIRMWARE_VERSION_PREFIX: u32 = 0x5690;

/// Decodes the version an MCU firmware image reports through
/// [`REG_FIRMWARE_VERSION`].
///
/// The CH569 core is RISC-V, which cannot materialise a 32-bit constant in one
/// instruction, so the version appears as `lui` followed by `addi` (older builds
/// use the compressed `c.addi`). Every PXView image from 1.34 to 1.5.8 does this
/// at exactly two call sites that agree with each other, and with
/// `#define FIRMWARE_VERSION` in the matching `pxlogic.h`.
///
/// Returns `None` unless the image yields exactly one distinct `0x5690_xxxx`
/// constant. An image that builds the constant some other way therefore falls
/// back to the caller's default instead of being guessed at.
pub fn declared_firmware_version(image: &[u8]) -> Option<u32> {
    let mut found: Option<u32> = None;
    let mut offset = 0;
    while offset + 4 <= image.len() {
        if let Some(version) = firmware_version_at(image, offset) {
            match found {
                None => found = Some(version),
                // Two sites disagree: refuse rather than pick one.
                Some(previous) if previous != version => return None,
                Some(_) => {}
            }
        }
        offset += 2;
    }
    found
}

/// Decodes a `lui`/`addi` constant pair starting at `offset`, if it builds a
/// firmware version.
fn firmware_version_at(image: &[u8], offset: usize) -> Option<u32> {
    let lui = read_u32_le(image, offset)?;
    // LUI: imm[31:12] | rd | 0110111
    if lui & 0x7f != 0x37 {
        return None;
    }
    let base = lui & 0xffff_f000;
    if base >> 16 != FIRMWARE_VERSION_PREFIX {
        return None;
    }
    let rd = (lui >> 7) & 0x1f;
    if rd == 0 {
        return None;
    }
    // The paired add is emitted within a couple of instructions of the `lui`.
    const PAIRED_ADD_SEARCH_BYTES: usize = 16;
    let mut cursor = offset + 4;
    while cursor < offset + 4 + PAIRED_ADD_SEARCH_BYTES {
        if let Some(imm) = addi_immediate(image, cursor, rd) {
            return Some(base.wrapping_add(imm as u32));
        }
        if let Some(imm) = compressed_addi_immediate(image, cursor, rd) {
            return Some(base.wrapping_add(imm as u32));
        }
        cursor += 2;
    }
    None
}

/// `addi rd, rd, imm12` where `rd` matches the preceding `lui`.
fn addi_immediate(image: &[u8], offset: usize, rd: u32) -> Option<i32> {
    let word = read_u32_le(image, offset)?;
    // OP-IMM with funct3 000 is ADDI.
    if word & 0x7f != 0x13 || (word >> 12) & 0x7 != 0 {
        return None;
    }
    if (word >> 7) & 0x1f != rd || (word >> 15) & 0x1f != rd {
        return None;
    }
    Some(sign_extend((word >> 20) & 0xfff, 12))
}

/// `c.addi rd, imm6` where `rd` matches the preceding `lui`. A zero immediate is
/// the canonical `c.nop`/HINT encoding and never part of a constant.
fn compressed_addi_immediate(image: &[u8], offset: usize, rd: u32) -> Option<i32> {
    let half = read_u16_le(image, offset)? as u32;
    // C.ADDI: 000 imm[5] rd imm[4:0] 01
    if half & 0x3 != 0x1 || half >> 13 != 0 {
        return None;
    }
    if (half >> 7) & 0x1f != rd {
        return None;
    }
    let imm = ((half >> 2) & 0x1f) | (((half >> 12) & 0x1) << 5);
    if imm == 0 {
        return None;
    }
    Some(sign_extend(imm, 6))
}

fn sign_extend(value: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((value << shift) as i32) >> shift
}

fn read_u32_le(image: &[u8], offset: usize) -> Option<u32> {
    image
        .get(offset..offset + 4)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u16_le(image: &[u8], offset: usize) -> Option<u16> {
    image
        .get(offset..offset + 2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
}

pub fn is_supported_pxlogic_id(vid: u16, pid: u16) -> bool {
    matches!(
        (vid, pid),
        (PXLOGIC_WCH_VID, PXLOGIC_WCH_PID) | (PXLOGIC_LEGACY_VID, PXLOGIC_LEGACY_PID)
    )
}

pub fn encode_register_write(addr: u32, value: u32) -> [u8; 16] {
    encode_register_packet(CMD_WRITE_REGISTER, addr, value)
}

pub fn encode_register_read(addr: u32) -> [u8; 16] {
    encode_register_packet(CMD_READ_REGISTER, addr, 0)
}

pub fn encode_register_packet(command: u32, addr: u32, value: u32) -> [u8; 16] {
    let mut packet = [0u8; 16];
    packet[0..4].copy_from_slice(&command.to_le_bytes());
    packet[4..8].copy_from_slice(&0x08u32.to_le_bytes());
    packet[8..12].copy_from_slice(&addr.to_le_bytes());
    packet[12..16].copy_from_slice(&value.to_le_bytes());
    packet
}

pub fn decode_register_value(packet: &[u8; 16]) -> u32 {
    u32::from_le_bytes(packet[12..16].try_into().expect("fixed slice"))
}

pub fn validate_register_ack(packet: &[u8; 16]) -> Result<()> {
    let ack = decode_register_value(packet);
    if ack == CMD_REGISTER_ACK {
        Ok(())
    } else {
        Err(CoreError::InvalidRegisterAck(ack))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_register_write_packet_little_endian() {
        let packet = encode_register_write(0x1122_3344, 0x5566_7788);
        assert_eq!(&packet[0..4], &CMD_WRITE_REGISTER.to_le_bytes());
        assert_eq!(&packet[4..8], &0x08u32.to_le_bytes());
        assert_eq!(&packet[8..12], &0x1122_3344u32.to_le_bytes());
        assert_eq!(&packet[12..16], &0x5566_7788u32.to_le_bytes());
    }

    #[test]
    fn validates_register_ack_word() {
        let mut packet = [0u8; 16];
        packet[12..16].copy_from_slice(&CMD_REGISTER_ACK.to_le_bytes());
        assert!(validate_register_ack(&packet).is_ok());
        packet[12..16].copy_from_slice(&0x1234u32.to_le_bytes());
        assert!(validate_register_ack(&packet).is_err());
    }

    /// `lui rd, 0x56900` — the RISC-V encoding PXView firmware uses to start
    /// building its version constant.
    fn lui(rd: u32, base: u32) -> [u8; 4] {
        ((base & 0xffff_f000) | (rd << 7) | 0x37).to_le_bytes()
    }

    /// `addi rd, rd, imm` (32-bit form, used by PXView 1.4.5 and later).
    fn addi(rd: u32, imm: u32) -> [u8; 4] {
        ((imm << 20) | (rd << 15) | (rd << 7) | 0x13).to_le_bytes()
    }

    /// `c.addi rd, imm` (compressed form, used by PXView 1.34 and 1.37).
    fn c_addi(rd: u32, imm: u32) -> [u8; 2] {
        (((imm & 0x20) << 7) as u16 | ((rd as u16) << 7) | (((imm & 0x1f) as u16) << 2) | 0x1)
            .to_le_bytes()
    }

    #[test]
    fn decodes_a_version_built_with_lui_and_addi() {
        let mut image = Vec::new();
        image.extend_from_slice(&lui(15, 0x5690_0000));
        image.extend_from_slice(&addi(15, 0x28));
        assert_eq!(declared_firmware_version(&image), Some(0x5690_0028));
    }

    #[test]
    fn decodes_a_version_built_with_a_compressed_addi() {
        let mut image = Vec::new();
        image.extend_from_slice(&lui(10, 0x5690_0000));
        image.extend_from_slice(&c_addi(10, 0x13));
        image.extend_from_slice(&[0, 0]);
        assert_eq!(declared_firmware_version(&image), Some(0x5690_0013));
    }

    #[test]
    fn tolerates_instructions_between_the_lui_and_its_addi() {
        let mut image = Vec::new();
        image.extend_from_slice(&lui(15, 0x5690_0000));
        image.extend_from_slice(&[0x13, 0x00, 0x00, 0x00]); // addi zero, zero, 0 (nop)
        image.extend_from_slice(&addi(15, 0x26));
        assert_eq!(declared_firmware_version(&image), Some(0x5690_0026));
    }

    #[test]
    fn ignores_immediates_that_are_not_firmware_versions() {
        let mut image = Vec::new();
        image.extend_from_slice(&lui(15, 0x1234_0000));
        image.extend_from_slice(&addi(15, 0x28));
        assert_eq!(declared_firmware_version(&image), None);
    }

    #[test]
    fn requires_the_addi_to_target_the_lui_register() {
        let mut image = Vec::new();
        image.extend_from_slice(&lui(15, 0x5690_0000));
        image.extend_from_slice(&addi(10, 0x28));
        assert_eq!(declared_firmware_version(&image), None);
    }

    #[test]
    fn refuses_an_image_whose_version_sites_disagree() {
        let mut image = Vec::new();
        image.extend_from_slice(&lui(15, 0x5690_0000));
        image.extend_from_slice(&addi(15, 0x28));
        image.extend_from_slice(&lui(10, 0x5690_0000));
        image.extend_from_slice(&addi(10, 0x27));
        assert_eq!(declared_firmware_version(&image), None);
    }

    #[test]
    fn accepts_an_image_whose_version_sites_agree() {
        let mut image = Vec::new();
        image.extend_from_slice(&lui(15, 0x5690_0000));
        image.extend_from_slice(&addi(15, 0x28));
        image.extend_from_slice(&lui(10, 0x5690_0000));
        image.extend_from_slice(&addi(10, 0x28));
        assert_eq!(declared_firmware_version(&image), Some(0x5690_0028));
    }

    #[test]
    fn falls_back_for_an_image_without_a_version_constant() {
        assert_eq!(declared_firmware_version(&[]), None);
        assert_eq!(declared_firmware_version(&[0xff; 64]), None);
    }

    #[test]
    fn never_reads_past_the_end_of_a_truncated_image() {
        // A `lui` with no room for its paired `addi` must not panic.
        let image = lui(15, 0x5690_0000);
        assert_eq!(declared_firmware_version(&image), None);
        for len in 0..image.len() {
            assert_eq!(declared_firmware_version(&image[..len]), None);
        }
    }

    /// Pins the decoder against the images the Bridge actually ships. Each file
    /// name carries the version PXView declares for it in
    /// `libsigrok/hardware/pxlogic/pxlogic.h`, so a decoder regression or a
    /// mislabelled resource fails here instead of at flash time.
    #[test]
    fn every_shipped_firmware_image_declares_the_version_its_name_claims() {
        let firmware_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("crate lives two levels below the repository root")
            .join("resources")
            .join("firmware");

        let expected: &[(&str, u32)] = &[
            ("SCI_LOGIC.bin", EXPECTED_FIRMWARE_VERSION),
            ("SCI_LOGIC-56900027.bin", 0x5690_0027),
            ("SCI_LOGIC-56900026.bin", 0x5690_0026),
            ("SCI_LOGIC-56900020.bin", 0x5690_0020),
        ];

        for (file_name, version) in expected {
            let path = firmware_dir.join(file_name);
            let image = std::fs::read(&path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
            assert_eq!(
                declared_firmware_version(&image),
                Some(*version),
                "{file_name} must declare 0x{version:08x}"
            );
        }
    }

    /// Every selectable image must be distinguishable, otherwise the Bridge
    /// cannot tell from `REG_FIRMWARE_VERSION` which one a device is running.
    #[test]
    fn shipped_firmware_versions_are_unique() {
        let firmware_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("crate lives two levels below the repository root")
            .join("resources")
            .join("firmware");

        let mut versions = Vec::new();
        for entry in std::fs::read_dir(&firmware_dir).expect("firmware directory") {
            let path = entry.expect("directory entry").path();
            if path.extension().and_then(|value| value.to_str()) != Some("bin") {
                continue;
            }
            let image = std::fs::read(&path).expect("firmware image");
            let version = declared_firmware_version(&image)
                .unwrap_or_else(|| panic!("{} declares no version", path.display()));
            assert!(
                !versions.contains(&version),
                "0x{version:08x} is declared by more than one shipped image"
            );
            versions.push(version);
        }
        assert!(
            versions.contains(&EXPECTED_FIRMWARE_VERSION),
            "the default image must remain available"
        );
    }
}
