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
pub const EXPECTED_FIRMWARE_VERSION: u32 = 0x5690_0028;
pub const BOOTLOADER_FIRMWARE_VERSION: u32 = 0x5690_0000;
pub const PWM_CLOCK_HZ: u32 = 125_000_000;
pub const PWM_MAX_FREQUENCY_HZ: f64 = 1_000_000.0;

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
}
