//! G13 direct-HID protocol: pure report packing/parsing.
//!
//! Proven contract from the LCDForge Go repair run (physical display
//! confirmed by the user on the target machine):
//!
//! - Vendor collection: VID 0x046D, PID 0xC21C, usage page 0xFF00, usage 0x0000
//! - Output report: 992 bytes = 1-byte report ID 0x03 + 32-byte header
//!   + 960-byte payload (160 x 48-bit storage rows, 43 rows visible)
//! - Bit packing: `report[32 + x + (y/8)*160] |= 1 << (y & 7)`
//! - Input report: 8 bytes, ID 0x01; LCD buttons in byte 6, bits 0x02 << i

use crate::render::Frame;

pub const G13_VENDOR_ID: u16 = 0x046d;
pub const G13_PRODUCT_ID: u16 = 0xc21c;
pub const G13_USAGE_PAGE: u16 = 0xff00;
pub const G13_USAGE: u16 = 0x0000;
pub const G13_INPUT_REPORT_LENGTH: usize = 8;
pub const G13_OUTPUT_REPORT_LENGTH: usize = 992;
pub const G13_LCD_HEADER_LENGTH: usize = 32;
#[allow(dead_code)]
pub const G13_LCD_STORAGE_HEIGHT: usize = 48;
#[allow(dead_code)]
pub const G13_LCD_PAYLOAD_LENGTH: usize = crate::model::WIDTH * G13_LCD_STORAGE_HEIGHT / 8;
pub const G13_LCD_REPORT_ID: u8 = 0x03;
pub const G13_INPUT_REPORT_ID: u8 = 0x01;

/// Pack a 160x43 logical frame into the 992-byte G13 output report.
pub fn pack_report(frame: &Frame) -> [u8; G13_OUTPUT_REPORT_LENGTH] {
    let mut report = [0u8; G13_OUTPUT_REPORT_LENGTH];
    report[0] = G13_LCD_REPORT_ID;
    for y in 0..crate::model::HEIGHT {
        for x in 0..crate::model::WIDTH {
            if frame.get(x as i32, y as i32) {
                let index = G13_LCD_HEADER_LENGTH + x + (y / 8) * crate::model::WIDTH;
                report[index] |= 1 << (y & 7);
            }
        }
    }
    report
}

/// All-off display report (used on close so the panel clears).
pub fn blank_report() -> [u8; G13_OUTPUT_REPORT_LENGTH] {
    let mut report = [0u8; G13_OUTPUT_REPORT_LENGTH];
    report[0] = G13_LCD_REPORT_ID;
    report
}

/// Parse the 8-byte input report into LCD button states (buttons 1..4).
pub fn parse_input(report: &[u8]) -> Result<[bool; 4], String> {
    if report.len() != G13_INPUT_REPORT_LENGTH {
        return Err(format!(
            "HID input report length {}, want {}",
            report.len(),
            G13_INPUT_REPORT_LENGTH
        ));
    }
    if report[0] != G13_INPUT_REPORT_ID {
        return Err(format!(
            "HID input report ID 0x{:02x}, want 0x{:02x}",
            report[0], G13_INPUT_REPORT_ID
        ));
    }
    let mut buttons = [false; 4];
    for (i, button) in buttons.iter_mut().enumerate() {
        *button = report[6] & (0x02u8 << i) != 0;
    }
    Ok(buttons)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_report_framing() {
        let blank = blank_report();
        assert_eq!(blank.len(), 992);
        assert_eq!(blank[0], 0x03);
        assert!(blank[1..].iter().all(|&b| b == 0));
    }

    #[test]
    fn pack_report_pixel_mapping() {
        let mut f = Frame::new();
        f.set(0, 0, true); // first pixel -> byte 32, bit 0
        f.set(159, 0, true); // last column of band 0 -> byte 32+159
        f.set(0, 7, true); // last row of band 0 -> byte 32+0, bit 7
        f.set(0, 8, true); // first row of band 1 -> byte 32+160, bit 0
        f.set(0, 42, true); // last visible row -> byte 32 + 5*160, bit 2
        let report = pack_report(&f);
        assert_eq!(report[0], 0x03);
        // y=0 and y=7 share byte 32: bits 0 and 7.
        assert_eq!(report[32], 0b1000_0001);
        assert_eq!(report[32 + 159], 0b0000_0001);
        assert_eq!(report[32 + 160], 0b0000_0001);
        assert_eq!(report[32 + 5 * 160], 0b0000_0100);
        // Nothing else set in the payload.
        let mut set_bytes = 0;
        for &b in &report[32..] {
            if b != 0 {
                set_bytes += 1;
            }
        }
        assert_eq!(set_bytes, 4);
    }

    #[test]
    fn pack_report_is_inverse_clean_for_all_on() {
        let mut f = Frame::new();
        for y in 0..43 {
            for x in 0..160 {
                f.set(x, y, true);
            }
        }
        let report = pack_report(&f);
        // Storage rows 43..48 (bits 3..7 of bands 5) must stay clear.
        for band in 0..5 {
            let base = 32 + band * 160;
            for x in 0..160 {
                assert_eq!(report[base + x], 0xFF, "band {} x {}", band, x);
            }
        }
        let base = 32 + 5 * 160;
        for x in 0..160 {
            assert_eq!(report[base + x], 0b0000_0111, "band 5 x {}", x);
        }
    }

    #[test]
    fn parse_input_button_bits() {
        // Byte 6 bits: 0x02<<i for button index i (0..4).
        // 0b1010 sets bits 1,3 -> button indices 0 and 2.
        let buttons = parse_input(&[0x01, 0, 0, 0, 0, 0, 0b1010, 0]).unwrap();
        assert_eq!(buttons, [true, false, true, false]);
        // 0x1A = 0b11010 sets bits 1,3,4 -> indices 0, 2, 3.
        let buttons = parse_input(&[0x01, 0, 0, 0, 0, 0, 0x1A, 0]).unwrap();
        assert_eq!(buttons, [true, false, true, true]);
        // 0x02 alone -> index 0.
        let buttons = parse_input(&[0x01, 0, 0, 0, 0, 0, 0x02, 0]).unwrap();
        assert_eq!(buttons, [true, false, false, false]);
    }

    #[test]
    fn parse_input_rejects_bad_framing() {
        assert!(parse_input(&[0u8; 7]).is_err(), "short report");
        assert!(parse_input(&[0u8; 9]).is_err(), "long report");
        let mut r = [0u8; 8];
        r[0] = 0x02;
        assert!(parse_input(&r).is_err(), "wrong report ID");
    }
}
