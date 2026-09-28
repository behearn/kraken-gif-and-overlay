//! USB and LCD details for the NZXT Kraken 2023 Elite.
//!
//! Other Kraken models are most likely to differ here: the USB id, the panel
//! size, the bulk header, and the HID reports that start a frame, commit it,
//! restore the liquid screen, or read the orientation. This was checked only
//! against a 2023 Elite (`1e71:300c`, 640×640).

use anyhow::{bail, Result};

/// NZXT vendor id.
pub(crate) const NZXT_VID: u16 = 0x1E71;
/// Kraken 2023 Elite product id.
pub(crate) const ELITE_PID: u16 = 0x300C;
/// Panel width in pixels. The Q565 encoder and the overlay both assume this.
pub(crate) const WIDTH: u32 = 640;
/// Panel height in pixels.
pub(crate) const HEIGHT: u32 = 640;

/// HID report that returns the panel to the built-in liquid temperature screen.
pub(crate) const LIQUID_SCREEN: [u8; 4] = [0x38, 0x01, 0x02, 0x00];

/// Ask the cooler for LCD brightness and orientation.
pub(crate) const LCD_QUERY: [u8; 2] = [0x30, 0x01];
/// Reply prefix for [`LCD_QUERY`].
pub(crate) const LCD_INFO_PREFIX: [u8; 2] = [0x31, 0x01];
pub(crate) const LCD_BRIGHTNESS_OFFSET: usize = 0x18;
/// Orientation step. `0` is 0°, `1` is 90°, `2` is 180°, `3` is 270°.
pub(crate) const LCD_ORIENTATION_OFFSET: usize = 0x1A;

/// Interrupt that prepares the cooler for one Q565 frame in mode `0x08`.
pub(crate) const FRAME_SETUP: [u8; 5] = [0x36, 0x01, 0x00, 0x01, 0x08];
pub(crate) const FRAME_SETUP_ACK: [u8; 2] = [0x37, 0x01];
/// Interrupt sent after the bulk header and payload.
pub(crate) const FRAME_COMMIT: [u8; 2] = [0x36, 0x02];
pub(crate) const FRAME_COMMIT_ACK: [u8; 2] = [0x37, 0x02];

const BULK_MAGIC: [u8; 12] = [
    0x12, 0xFA, 0x01, 0xE8, 0xAB, 0xCD, 0xEF, 0x98, 0x76, 0x54, 0x32, 0x10,
];

/// 20-byte bulk header: magic, mode `0x08`, then the payload length.
pub(crate) fn bulk_header(payload_len: usize) -> [u8; 20] {
    let mut header = [0u8; 20];
    header[..12].copy_from_slice(&BULK_MAGIC);
    header[12] = 0x08;
    header[16..20].copy_from_slice(&(payload_len as u32).to_le_bytes());
    header
}

/// HID report that stores brightness and orientation without changing brightness.
pub(crate) fn orientation_report(brightness: u8, degrees: u16) -> [u8; 8] {
    [0x30, 0x02, 0x01, brightness, 0x00, 0x00, 0x01, (degrees / 90) as u8]
}

pub(crate) fn orientation_degrees(step: u8) -> Result<u16> {
    match step {
        0..=3 => Ok(u16::from(step) * 90),
        other => bail!("Unexpected LCD orientation {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_header_carries_mode_and_length() {
        let header = bulk_header(0x0102_0304);
        assert_eq!(&header[..12], &BULK_MAGIC);
        assert_eq!(header[12], 0x08);
        assert_eq!(&header[16..20], &0x0102_0304u32.to_le_bytes());
    }

    #[test]
    fn orientation_report_keeps_brightness_and_stores_quarter_turns() {
        assert_eq!(
            orientation_report(40, 270),
            [0x30, 0x02, 0x01, 40, 0x00, 0x00, 0x01, 3]
        );
    }

    #[test]
    fn orientation_step_is_a_quarter_turn() {
        assert_eq!(orientation_degrees(0).unwrap(), 0);
        assert_eq!(orientation_degrees(2).unwrap(), 180);
        assert!(orientation_degrees(4).is_err());
    }
}
