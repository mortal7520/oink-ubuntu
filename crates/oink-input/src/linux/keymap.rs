//! Physical keys use XKB key names, not layout-dependent keysyms. The evdev
//! table also covers consumer keys and is the fallback for standard Xorg evdev.
use crate::KeyCode;
const KEYS: &[(u16, u16)] = &[
    (1, 0x29),
    (2, 0x1e),
    (3, 0x1f),
    (4, 0x20),
    (5, 0x21),
    (6, 0x22),
    (7, 0x23),
    (8, 0x24),
    (9, 0x25),
    (10, 0x26),
    (11, 0x27),
    (12, 0x2d),
    (13, 0x2e),
    (14, 0x2a),
    (15, 0x2b),
    (16, 0x14),
    (17, 0x1a),
    (18, 0x08),
    (19, 0x15),
    (20, 0x17),
    (21, 0x1c),
    (22, 0x18),
    (23, 0x0c),
    (24, 0x12),
    (25, 0x13),
    (26, 0x2f),
    (27, 0x30),
    (28, 0x28),
    (29, 0xe0),
    (30, 0x04),
    (31, 0x16),
    (32, 0x07),
    (33, 0x09),
    (34, 0x0a),
    (35, 0x0b),
    (36, 0x0d),
    (37, 0x0e),
    (38, 0x0f),
    (39, 0x33),
    (40, 0x34),
    (41, 0x35),
    (42, 0xe1),
    (43, 0x31),
    (44, 0x1d),
    (45, 0x1b),
    (46, 0x06),
    (47, 0x19),
    (48, 0x05),
    (49, 0x11),
    (50, 0x10),
    (51, 0x36),
    (52, 0x37),
    (53, 0x38),
    (54, 0xe5),
    (55, 0x55),
    (56, 0xe2),
    (57, 0x2c),
    (58, 0x39),
    (59, 0x3a),
    (60, 0x3b),
    (61, 0x3c),
    (62, 0x3d),
    (63, 0x3e),
    (64, 0x3f),
    (65, 0x40),
    (66, 0x41),
    (67, 0x42),
    (68, 0x43),
    (69, 0x53),
    (70, 0x47),
    (71, 0x5f),
    (72, 0x60),
    (73, 0x61),
    (74, 0x56),
    (75, 0x5c),
    (76, 0x5d),
    (77, 0x5e),
    (78, 0x57),
    (79, 0x59),
    (80, 0x5a),
    (81, 0x5b),
    (82, 0x62),
    (83, 0x63),
    (86, 0x64),
    (87, 0x44),
    (88, 0x45),
    (96, 0x58),
    (97, 0xe4),
    (98, 0x54),
    (99, 0x46),
    (100, 0xe6),
    (102, 0x4a),
    (103, 0x52),
    (104, 0x4b),
    (105, 0x50),
    (106, 0x4f),
    (107, 0x4d),
    (108, 0x51),
    (109, 0x4e),
    (110, 0x49),
    (111, 0x4c),
    (116, 0x66),
    (119, 0x48),
    (125, 0xe3),
    (126, 0xe7),
    (127, 0x65),
    (183, 0x68),
    (184, 0x69),
    (185, 0x6a),
    (186, 0x6b),
    (187, 0x6c),
    (188, 0x6d),
    (189, 0x6e),
    (190, 0x6f),
    (191, 0x70),
    (192, 0x71),
    (193, 0x72),
    (194, 0x73),
];
const CONSUMER: &[(u16, u16)] = &[
    (113, 0xe2),
    (114, 0xea),
    (115, 0xe9),
    (163, 0xb5),
    (164, 0xcd),
    (165, 0xb6),
    (166, 0xb7),
    (161, 0xb8),
    (224, 0x70),
    (225, 0x6f),
];
pub fn from_evdev(code: u16) -> Option<KeyCode> {
    KEYS.iter()
        .find(|&&(n, _)| n == code)
        .map(|&(_, u)| KeyCode::keyboard(u))
        .or_else(|| {
            CONSUMER
                .iter()
                .find(|&&(n, _)| n == code)
                .map(|&(_, u)| KeyCode::new(KeyCode::PAGE_CONSUMER, u))
        })
}
pub fn to_evdev(code: KeyCode) -> Option<u16> {
    let table = match code.page() {
        KeyCode::PAGE_KEYBOARD => KEYS,
        KeyCode::PAGE_CONSUMER => CONSUMER,
        _ => return None,
    };
    table.iter().find(|&&(_, u)| u == code.usage()).map(|&(n, _)| n)
}
pub fn from_xkb_name(name: [u8; 4]) -> Option<KeyCode> {
    let s = std::str::from_utf8(&name).ok()?.trim_end_matches('\0');
    for (prefix, usages) in [
        (
            "AE",
            &[0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x2d, 0x2e][..],
        ),
        (
            "AD",
            &[0x14, 0x1a, 0x08, 0x15, 0x17, 0x1c, 0x18, 0x0c, 0x12, 0x13, 0x2f, 0x30][..],
        ),
        (
            "AC",
            &[0x04, 0x16, 0x07, 0x09, 0x0a, 0x0b, 0x0d, 0x0e, 0x0f, 0x33, 0x34][..],
        ),
        ("AB", &[0x1d, 0x1b, 0x06, 0x19, 0x05, 0x11, 0x10, 0x36, 0x37, 0x38][..]),
    ] {
        if let Some(t) = s.strip_prefix(prefix) {
            let n = t.parse::<usize>().ok()?;
            return n
                .checked_sub(1)
                .and_then(|i| usages.get(i))
                .map(|&u| KeyCode::keyboard(u));
        }
    }
    if let Some(t) = s.strip_prefix("FK") {
        let n = t.parse::<u16>().ok()?;
        return match n {
            1..=12 => Some(KeyCode::keyboard(0x39 + n)),
            13..=24 => Some(KeyCode::keyboard(0x68 + n - 13)),
            _ => None,
        };
    }
    if let Some(t) = s.strip_prefix('I') {
        return t.parse::<u16>().ok()?.checked_sub(8).and_then(from_evdev);
    }
    let usage = match s {
        "ESC" => 0x29,
        "BKSP" => 0x2a,
        "TAB" => 0x2b,
        "RTRN" => 0x28,
        "SPCE" => 0x2c,
        "TLDE" => 0x35,
        "BKSL" => 0x31,
        "LSGT" => 0x64,
        "CAPS" => 0x39,
        "LCTL" => 0xe0,
        "LFSH" => 0xe1,
        "LALT" => 0xe2,
        "LWIN" => 0xe3,
        "RCTL" => 0xe4,
        "RTSH" => 0xe5,
        "RALT" => 0xe6,
        "RWIN" => 0xe7,
        "MENU" => 0x65,
        "PRSC" => 0x46,
        "SCLK" => 0x47,
        "PAUS" => 0x48,
        "INS" => 0x49,
        "HOME" => 0x4a,
        "PGUP" => 0x4b,
        "DELE" => 0x4c,
        "END" => 0x4d,
        "PGDN" => 0x4e,
        "RGHT" => 0x4f,
        "LEFT" => 0x50,
        "DOWN" => 0x51,
        "UP" => 0x52,
        "NMLK" => 0x53,
        "KPDV" => 0x54,
        "KPMU" => 0x55,
        "KPSU" => 0x56,
        "KPAD" => 0x57,
        "KPEN" => 0x58,
        "KP1" => 0x59,
        "KP2" => 0x5a,
        "KP3" => 0x5b,
        "KP4" => 0x5c,
        "KP5" => 0x5d,
        "KP6" => 0x5e,
        "KP7" => 0x5f,
        "KP8" => 0x60,
        "KP9" => 0x61,
        "KP0" => 0x62,
        "KPDL" => 0x63,
        "MUTE" => return Some(KeyCode::new(0x0c, 0xe2)),
        "VOL-" => return Some(KeyCode::new(0x0c, 0xea)),
        "VOL+" => return Some(KeyCode::new(0x0c, 0xe9)),
        _ => return None,
    };
    Some(KeyCode::keyboard(usage))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_rows_do_not_depend_on_layout_symbols() {
        assert_eq!(from_xkb_name(*b"AD01"), Some(KeyCode::keyboard(0x14)));
        assert_eq!(from_xkb_name(*b"AC01"), Some(KeyCode::keyboard(0x04)));
        assert_eq!(from_xkb_name(*b"RALT"), Some(KeyCode::RIGHT_ALT));
    }
    #[test]
    fn evdev_roundtrip() {
        for &(evdev, _) in KEYS.iter().chain(CONSUMER) {
            let hid = from_evdev(evdev).unwrap();
            assert_eq!(to_evdev(hid), Some(evdev));
        }
    }
    #[test]
    fn unsupported_keys_are_not_claimed() {
        assert_eq!(to_evdev(KeyCode::APPLE_FN), None);
        assert_eq!(from_xkb_name(*b"XXXX"), None);
    }
}
