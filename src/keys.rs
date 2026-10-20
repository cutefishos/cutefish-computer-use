// SPDX-FileCopyrightText: 2026 Reion Wong <reionwong@gmail.com>
// SPDX-License-Identifier: BSD-3-Clause

// Key names as agents write them: "ctrl+shift+t", "Return".

// Linux evdev codes, from linux/input-event-codes.h.
pub const KEY_LEFTCTRL: u32 = 29;
pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_LEFTALT: u32 = 56;
pub const KEY_LEFTMETA: u32 = 125;

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    // A key by its position, the same on every layout.
    Code(u32),
    // Whichever key types the keysym on the current layout.
    Sym(u32),
}

const NAMED_CODES: &[(&[&str], u32)] = &[
    (&["ctrl", "control", "control_l", "ctrl_l"], KEY_LEFTCTRL),
    (&["control_r", "ctrl_r"], 97),
    (&["shift", "shift_l"], KEY_LEFTSHIFT),
    (&["shift_r"], 54),
    (&["alt", "alt_l", "option"], KEY_LEFTALT),
    (&["alt_r"], 100),
    (&["super", "super_l", "meta", "meta_l", "cmd", "command", "win"], KEY_LEFTMETA),
    (&["super_r", "meta_r"], 126),
    (&["return", "enter"], 28),
    (&["kp_enter"], 96),
    (&["tab"], 15),
    (&["escape", "esc"], 1),
    (&["backspace"], 14),
    (&["delete", "del"], 111),
    (&["insert"], 110),
    (&["home"], 102),
    (&["end"], 107),
    (&["page_up", "pageup", "prior"], 104),
    (&["page_down", "pagedown", "next"], 109),
    (&["up"], 103),
    (&["down"], 108),
    (&["left"], 105),
    (&["right"], 106),
    (&["space"], 57),
    (&["caps_lock", "capslock"], 58),
    (&["num_lock"], 69),
    (&["scroll_lock"], 70),
    (&["print"], 99),
    (&["pause"], 119),
    (&["menu"], 127),
    (&["xf86audiomute"], 113),
    (&["xf86audiolowervolume"], 114),
    (&["xf86audioraisevolume"], 115),
    (&["xf86launcha"], 120),
    (&["xf86audionext"], 163),
    (&["xf86audioplay", "xf86audiopause"], 164),
    (&["xf86audioprev"], 165),
    (&["xf86monbrightnessdown"], 224),
    (&["xf86monbrightnessup"], 225),
    (&["xf86kbdbrightnessdown"], 229),
    (&["xf86kbdbrightnessup"], 230),
    (&["xf86audiomicmute"], 248),
];

const NAMED_SYMS: &[(&str, char)] = &[
    ("minus", '-'),
    ("equal", '='),
    ("plus", '+'),
    ("comma", ','),
    ("period", '.'),
    ("slash", '/'),
    ("backslash", '\\'),
    ("semicolon", ';'),
    ("apostrophe", '\''),
    ("grave", '`'),
    ("bracketleft", '['),
    ("bracketright", ']'),
];

fn function_key(name: &str) -> Option<u32> {
    let number: u32 = name.strip_prefix('f')?.parse().ok()?;
    match number {
        1..=10 => Some(58 + number),
        11 => Some(87),
        12 => Some(88),
        13..=24 => Some(170 + number),
        _ => None,
    }
}

// Latin-1 keysyms are their code points; the rest of Unicode is offset.
fn keysym(character: char) -> u32 {
    let code = character as u32;
    if (0x20..=0x7e).contains(&code) || (0xa0..=0xff).contains(&code) {
        code
    } else {
        0x0100_0000 + code
    }
}

fn token(name: &str) -> Result<Vec<Key>, String> {
    let mut characters = name.chars();
    if let (Some(character), None) = (characters.next(), characters.next()) {
        // A capital letter is the letter with Shift.
        if character.is_uppercase() {
            let lower: Vec<char> = character.to_lowercase().collect();
            if lower.len() == 1 {
                return Ok(vec![Key::Code(KEY_LEFTSHIFT), Key::Sym(keysym(lower[0]))]);
            }
        }
        return Ok(vec![Key::Sym(keysym(character))]);
    }
    let lower = name.to_ascii_lowercase();
    if let Some((_, code)) = NAMED_CODES.iter().find(|(names, _)| names.contains(&lower.as_str())) {
        return Ok(vec![Key::Code(*code)]);
    }
    if let Some(code) = function_key(&lower) {
        return Ok(vec![Key::Code(code)]);
    }
    if let Some((_, character)) = NAMED_SYMS.iter().find(|(symbol, _)| *symbol == lower) {
        return Ok(vec![Key::Sym(keysym(*character))]);
    }
    Err(format!("Unknown key \"{name}\"."))
}

// The keys of a chord in the order they go down; they come up in reverse.
pub fn parse_chord(chord: &str) -> Result<Vec<Key>, String> {
    let chord = chord.trim();
    if chord.is_empty() {
        return Err("No keys given.".into());
    }
    let (modifiers, last) = if chord == "+" {
        ("", "+")
    } else if let Some(modifiers) = chord.strip_suffix("++") {
        (modifiers, "+")
    } else {
        chord.rsplit_once('+').unwrap_or(("", chord))
    };
    let mut keys = Vec::new();
    for name in modifiers.split('+').map(str::trim).filter(|name| !name.is_empty()) {
        keys.extend(token(name)?);
    }
    keys.extend(token(last.trim())?);
    let mut unique = Vec::new();
    for key in keys {
        if !unique.contains(&key) {
            unique.push(key);
        }
    }
    Ok(unique)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_are_positions_and_characters_are_keysyms() {
        assert_eq!(
            parse_chord("ctrl+shift+t").unwrap(),
            vec![Key::Code(KEY_LEFTCTRL), Key::Code(KEY_LEFTSHIFT), Key::Sym('t' as u32)]
        );
        assert_eq!(parse_chord("Return").unwrap(), vec![Key::Code(28)]);
        assert_eq!(parse_chord("super").unwrap(), vec![Key::Code(KEY_LEFTMETA)]);
        assert_eq!(parse_chord("F12").unwrap(), vec![Key::Code(88)]);
        assert_eq!(parse_chord("F13").unwrap(), vec![Key::Code(183)]);
    }

    #[test]
    fn a_capital_letter_adds_shift_once() {
        assert_eq!(
            parse_chord("shift+T").unwrap(),
            vec![Key::Code(KEY_LEFTSHIFT), Key::Sym('t' as u32)]
        );
    }

    #[test]
    fn plus_is_a_key_too() {
        assert_eq!(parse_chord("+").unwrap(), vec![Key::Sym('+' as u32)]);
        assert_eq!(
            parse_chord("ctrl++").unwrap(),
            vec![Key::Code(KEY_LEFTCTRL), Key::Sym('+' as u32)]
        );
        assert_eq!(
            parse_chord("ctrl+minus").unwrap(),
            vec![Key::Code(KEY_LEFTCTRL), Key::Sym('-' as u32)]
        );
    }

    #[test]
    fn beyond_latin1_keysyms_are_offset() {
        assert_eq!(parse_chord("ä").unwrap(), vec![Key::Sym(0xe4)]);
        assert_eq!(parse_chord("я").unwrap(), vec![Key::Sym(0x0100_044f)]);
        assert!(parse_chord("hyper").is_err());
        assert!(parse_chord("").is_err());
    }
}
