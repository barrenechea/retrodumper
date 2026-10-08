//! Homebrew flash image (`0x459200` when the mapper is 8).
//!
//! The file is stored at offset `0x80000`. The bytes in front of it are the
//! client's menu, plus a config page filled from a side copy of the file
//! (`0x463d30`). That copy may drop a 512-byte copier header; the bytes at
//! `0x80000` stay the file the user passed.

const MENU_PAD: usize = 0x8_0000;
const MENU_LO: &[u8] = include_bytes!("homebrew_menu.bin");
const MENU_TAIL: &[u8] = include_bytes!("homebrew_tail.bin");

const _: () = assert!(MENU_LO.len() == 0x2000);
const _: () = assert!(MENU_TAIL.len() == 0x50);

struct Detected {
    kind: u8,
    rom_size: u32,
    ram: u32,
}

pub(super) fn image(file: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; file.len() + MENU_PAD];
    out[MENU_PAD..].copy_from_slice(file);
    out[..MENU_LO.len()].copy_from_slice(MENU_LO);
    out[0x8000..0x8000 + MENU_LO.len()].copy_from_slice(MENU_LO);
    out[0x7FB0..0x7FB0 + MENU_TAIL.len()].copy_from_slice(MENU_TAIL);
    out[0xFFB0..0xFFB0 + MENU_TAIL.len()].copy_from_slice(MENU_TAIL);
    let info = detect(file);
    paint(&mut out, info.kind, info.rom_size, info.ram);
    out
}

fn detect(file: &[u8]) -> Detected {
    let n = file.len().min(0x0100_0000);
    if n == 0 {
        return Detected {
            kind: 0,
            rom_size: 0,
            ram: 0,
        };
    }
    let mut rom = file[..n].to_vec();
    let mut size = u32::try_from(n).unwrap_or(u32::MAX);
    if has_copier(&rom, size) {
        size = size.wrapping_sub(0x200);
        if u64::from(size) <= n as u64 {
            let keep = size as usize;
            rom.copy_within(0x200..0x200 + keep, 0);
            rom.truncate(keep);
        } else {
            rom.clear();
        }
    }
    if size == 0 {
        return Detected {
            kind: 0,
            rom_size: 0,
            ram: 0,
        };
    }
    let (kind, header) = classify(&mut rom, size);
    let ram = ram_bytes(&rom, header);
    Detected {
        kind,
        rom_size: size,
        ram,
    }
}

/// Copier-header predicate at the top of `0x463d30`.
fn has_copier(buf: &[u8], size: u32) -> bool {
    if buf.starts_with(b"GAME DOCTOR SF 3") {
        return true;
    }
    if buf.len() >= 16 && &buf[8..16] == b"SUPERUFO" {
        return true;
    }
    if size & 0x7FFF == 0 {
        return false;
    }
    if size & 0x7FFF == 0x200 {
        return true;
    }
    if buf.len() >= 11 && buf[8] == 0xAA && buf[9] == 0xBB && buf[10] == 0x04 {
        return true;
    }
    let b4 = byte_at(buf, 4);
    let b5 = byte_at(buf, 5);
    match b4 {
        0x77 => b5 == 0x83,
        0xDD => b5 == 0x82 || b5 == 0x02,
        0xF7 => b5 == 0x83,
        0xFD => b5 == 0x82,
        0x00 => b5 == 0x80,
        b'G' => b5 == 0x83,
        0x11 => b5 == 0x82 || b5 == 0x02,
        _ => false,
    }
}

fn classify(rom: &mut [u8], size: u32) -> (u8, usize) {
    let mut kind = 0u8;
    let mut header = 0usize;
    if size < 0xFFC0 {
        kind = 1;
        header = 0x7FC0;
    }
    let banks = size >> 15;
    if size < 0x50_0000 {
        if header == 0 {
            maybe_split(rom, size);
            (kind, header) = pick_header(rom);
        }
    } else {
        // The wide split mutates the side copy before the ExHiROM header test.
        if sum_pair(rom, 0x7FDC) == 0xFFFF && matches!(byte_at(rom, 0x7FD5), b'5' | b'%') {
            split_wide(rom, banks);
        }
        if sum_pair(rom, 0x40_FFDC) == 0xFFFF && matches!(byte_at(rom, 0x40_FFD5), b'5' | b'%') {
            kind = 2;
            header = 0x40_FFC0;
        } else if header == 0 {
            maybe_split(rom, size);
            (kind, header) = pick_header(rom);
        }
    }
    if size == 0xC0_0000 {
        kind = 4;
    } else if header == 0x40_FFC0 {
        kind = 3;
    }
    (kind, header)
}

/// `0x463720` / `0x463c50` on a ROM at least 5 MB, run on the side copy only.
fn split_wide(rom: &mut [u8], banks: u32) {
    deinterleave(rom, 0x80);
    if rom.len() > 0x10_0000 {
        deinterleave(&mut rom[0x10_0000..], banks.saturating_sub(0x80));
    }
}

fn maybe_split(rom: &mut [u8], size: u32) {
    let half = (size >> 1) as usize;
    let half_sum = sum_pair(rom, half + 0x7FDC);
    let base_sum = sum_pair(rom, 0x7FDC);
    let half_country = byte_at(rom, half + 0x7FD9);
    if half_sum != 0xFFFF || base_sum == 0xFFFF || half_country > 0x0D {
        if base_sum != 0xFFFF || sum_pair(rom, 0xFFDC) == 0xFFFF || byte_at(rom, 0x7FD9) > 0x0D {
            return;
        }
        let map = byte_at(rom, 0x7FD5);
        if !matches!(map, b'!' | b'1' | b'5' | b':') {
            return;
        }
        let title = byte_at(rom, 0x7FD4);
        if title != b' '
            && (map == title
                || map == byte_at(rom, 0x7FD3)
                || map == byte_at(rom, 0x7FD2)
                || map == byte_at(rom, 0x7FD1))
        {
            return;
        }
    }
    deinterleave(rom, size >> 15);
}

fn deinterleave(rom: &mut [u8], banks: u32) {
    let banks = banks as usize;
    let half = banks >> 1;
    if half == 0 {
        return;
    }
    let mut table = vec![0u8; half * 2];
    for i in 0..half {
        table[i * 2] = (half as u8).wrapping_add(i as u8);
        table[i * 2 + 1] = i as u8;
    }
    for cur in 0..banks {
        let Some(j) = table.iter().position(|e| *e == cur as u8) else {
            continue;
        };
        let src = table[j];
        let partner = table.get(cur).copied().unwrap_or(0);
        // `movsx` then `shl 0xf`. A bank byte >= 0x80 is before the copy.
        swap_banks(rom, src, partner);
        table[j] = partner;
        if cur < table.len() {
            table[cur] = src;
        }
    }
}

fn bank_off(index: u8, len: usize) -> Option<usize> {
    let bank = isize::from(index as i8);
    if bank < 0 {
        return None;
    }
    let off = (bank as usize) * 0x8000;
    if off + 0x8000 > len { None } else { Some(off) }
}

fn swap_banks(rom: &mut [u8], a: u8, b: u8) {
    let Some(a_off) = bank_off(a, rom.len()) else {
        return;
    };
    let Some(b_off) = bank_off(b, rom.len()) else {
        return;
    };
    if a_off == b_off {
        return;
    }
    let (lo, hi) = if a_off < b_off {
        (a_off, b_off)
    } else {
        (b_off, a_off)
    };
    let (left, right) = rom.split_at_mut(hi);
    left[lo..lo + 0x8000].swap_with_slice(&mut right[..0x8000]);
}

fn pick_header(rom: &[u8]) -> (u8, usize) {
    let mut lo = header_score(rom, 0x7FC0);
    let mut hi = header_score(rom, 0xFFC0);
    if matches!(byte_at(rom, 0x7FD5), b' ' | b'#' | b'0' | b'2') {
        lo += 3;
    }
    if matches!(byte_at(rom, 0xFFD5), b'!' | b'1' | b'5' | b':') {
        hi += 3;
    }
    if lo < hi { (2, 0xFFC0) } else { (1, 0x7FC0) }
}

/// Header score from `0x463870`, including the reset-vector check `0x464470`.
fn header_score(rom: &[u8], header: usize) -> i32 {
    let mut score = reset_score(rom, header);
    if sum_pair(rom, header + 0x1C) == 0xFFFF {
        score += 5;
    }
    if byte_at(rom, header + 0x1A) == b'3' {
        score += 3;
    }
    if byte_at(rom, header + 0x17) == 0 {
        score += 2;
    }
    // `shl` / `cmp` / `cmovle`: the penalty is a signed compare. `1<<31`
    // and `8<<28` stay unpenalized.
    let shift = u32::from(byte_at(rom, header + 0x17).wrapping_sub(7) & 0x1F);
    if ((1u32 << shift) as i32) > 0x30 {
        score -= 2;
    }
    if ((8u32 << (byte_at(rom, header + 0x18) & 0x1F)) as i32) > 0x400 {
        score -= 2;
    }
    if byte_at(rom, header + 0x19) <= 0x0D {
        score += 2;
    }
    // `cmp ecx, 0x14` / `jl` — twenty bytes, indexes 0..=0x13.
    let printable = (0..0x14).all(|i| {
        let c = byte_at(rom, header + i);
        c == 0 || c.wrapping_sub(0x20) < 0x5F
    });
    if !printable {
        score -= 2;
    }
    if matches!(byte_at(rom, header + 0x18), 0x20 | 0x21 | 0x30 | 0x31) {
        score += 2;
    }
    score
}

fn reset_score(rom: &[u8], header: usize) -> i32 {
    let vector = u16::from_le_bytes([byte_at(rom, header + 0x3C), byte_at(rom, header + 0x3D)]);
    if vector == 0xFFFF || vector < 0x8000 {
        return -4;
    }
    let at = |add: usize| byte_at(rom, (usize::from(vector).wrapping_add(add)) & 0x7FFF);
    let (op, a, b) = (at(0), at(1), at(2));
    let ok = match op {
        0x18 | 0x5C | 0x78 | 0xAD => true,
        0x20 => (a == 0x16 || a == 0x06) && b == 0x80,
        0x4B => a == 0xAB && (b == 0x18 || b == b' '),
        0x4C => {
            if (a == 0 || a == 0xC0) && b == 0x84 {
                true
            } else if a == b'm' {
                b == 0x86
            } else if a == 0 {
                b == 0x80
            } else {
                return 2;
            }
        }
        0x80 => {
            if a == 0x16 {
                b == b'L'
            } else if a == 0x07 {
                b == 0x82
            } else {
                return 2;
            }
        }
        0x9C => a == 0 && b == b'!',
        0xA2 => a == 0xFF && b == 0x86,
        0xA9 => {
            if a == 0 {
                true
            } else if a == 0x8F {
                b == 0x8D
            } else if a == b' ' || a == 0x1F {
                b == b'K'
            } else {
                return 2;
            }
        }
        0xC2 => a == b'0' && b == 0xA9,
        _ => return 2,
    };
    if ok { 10 } else { 2 }
}

fn ram_bytes(rom: &[u8], header: usize) -> u32 {
    let map = byte_at(rom, header + 0x15);
    let chip = byte_at(rom, header + 0x16);
    let (no_ram, a96, a97) = match preset_flags(map, chip) {
        Some(flags) => flags,
        None => (
            forces_no_sram(
                map,
                chip,
                byte_at(rom, header + 0x17),
                byte_at(rom, header + 0x1A),
                byte_at(rom, header + 0x18),
            ),
            false,
            false,
        ),
    };
    if no_ram {
        return 0;
    }
    if a96 {
        return if byte_at(rom, header + 0x1A) == b'3' {
            sram_bytes(byte_at(rom, header.wrapping_sub(3)))
        } else {
            0x8000
        };
    }
    if a97 {
        return 0x1000;
    }
    if rom.starts_with(b"BANDAI SFC-ADX") {
        return sram_bytes(byte_at(rom, 0x10_0032));
    }
    let sram = byte_at(rom, header + 0x18);
    if sram == 0 { 0 } else { sram_bytes(sram) }
}

/// `(no_sram, a96, a97)` when `0x4639b0` returns before its final test.
fn preset_flags(map: u8, chip: u8) -> Option<(bool, bool, bool)> {
    if chip == 3 || chip == 5 {
        return Some((false, false, false));
    }
    let w = (u16::from(chip) << 8) | u16::from(map);
    if w < 0x4333 {
        if w == 0x4332 {
            return Some((false, false, false));
        }
        if w < 0x2531 {
            if w == 0x2530 {
                return Some((false, false, false));
            }
            if w < 0x1521 {
                if w == 0x1520 || w == 0x1320 || w == 0x1420 {
                    return Some((false, true, false));
                }
            } else if w == 0x1A20 {
                return Some((false, true, false));
            }
        } else if w == 0x3223 || w == 0x3423 || w == 0x3523 {
            return Some((false, false, false));
        }
    } else if w > 0xF530 {
        if w == 0xF630 {
            return Some((false, false, true));
        }
        if w == 0xF53A || w == 0xF93A {
            return Some((false, false, false));
        }
    } else if w == 0xF530 || w == 0xE320 || w == 0x4532 || w == 0x5535 || w == 0xF320 {
        return Some((false, false, false));
    }
    None
}

fn forces_no_sram(map: u8, chip: u8, rom_b: u8, licensee: u8, sram: u8) -> bool {
    let license_ok = licensee == b'3' || licensee == 0xFF;
    let map_ok = map == 0 || (map & 0x83) == 0x80;
    let sram_ok = matches!(sram, 0x20 | 0x21 | 0x30 | 0x31);
    if !(license_ok && map_ok && sram_ok) {
        return false;
    }
    if chip == 0 && rom_b == 0 {
        return false;
    }
    if chip == 0xFF && rom_b == 0xFF {
        return true;
    }
    (chip & 0x0F) == 0 && (chip & 0xF0) < 0xD0
}

fn sram_bytes(code: u8) -> u32 {
    (8u32 << (code & 0x1F)).min(0x400) * 0x80
}

fn rom_slot(size: u32) -> u8 {
    if size < 0x8_0001 {
        0
    } else if size < 0x10_0001 {
        1
    } else if size < 0x20_0001 {
        3
    } else {
        7
    }
}

fn paint(out: &mut [u8], kind: u8, rom_size: u32, ram: u32) {
    out[0x603D] = 1;
    if ram != 0 {
        out[0x603F] = 1;
        out[0x603E] = if ram == 0x800 {
            0
        } else if ram == 0x2000 {
            3
        } else {
            0x0F
        };
    }
    out[0x603C] = rom_slot(rom_size);
    if kind == 2 {
        out[0x603F] |= 0x10;
    }
    if kind == 3 {
        out[0x603F] |= 0x20;
    }
    out[0x6000] = 0xFF;
    out[0x6006] = 0x10;
    out[0x600C] = 0x30;
    out[0x600D] = 0x31;
    out[0x600E] = 0x2E;
    out[0x600F] = 0x20;
    out[0x6010] = 0x32;
    out.copy_within(0x6000..0x7000, 0xE000);
}

fn sum_pair(buf: &[u8], off: usize) -> u32 {
    let b = |n| u32::from(byte_at(buf, off + n));
    (b(3) + b(1)) * 0x100 + b(2) + b(0)
}

fn byte_at(buf: &[u8], i: usize) -> u8 {
    buf.get(i).copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rom(len: usize, sets: &[(usize, u8)]) -> Vec<u8> {
        let mut v = vec![0u8; len];
        for &(i, b) in sets {
            v[i] = b;
        }
        v
    }

    fn assert_menu(out: &[u8], file: &[u8]) {
        assert_eq!(out.len(), file.len() + MENU_PAD);
        assert_eq!(&out[..0x2000], MENU_LO);
        assert_eq!(&out[0x8000..0xA000], MENU_LO);
        assert_eq!(&out[0x7FB0..0x8000], MENU_TAIL);
        assert_eq!(&out[0xFFB0..0x10000], MENU_TAIL);
        assert_eq!(&out[0x7FC0..0x7FC4], b"MENU");
        assert_eq!(&out[MENU_PAD..], file);
        assert_eq!(&out[0xE000..0xF000], &out[0x6000..0x7000]);
        assert_eq!(out[0x6000], 0xFF);
        assert_eq!(out[0x6006], 0x10);
        assert_eq!(&out[0x600C..0x6011], &[0x30, 0x31, 0x2E, 0x20, 0x32]);
        assert_eq!(out[0x603D], 1);
    }

    #[test]
    fn lorom_sram_selects_the_config_byte() {
        let file = rom(0x8000, &[(0x7FD8, 1)]);
        let out = image(&file);
        assert_menu(&out, &file);
        assert_eq!(out[0x603C], 0);
        assert_eq!(out[0x603E], 0);
        assert_eq!(out[0x603F], 1);

        let file = rom(0x8000, &[(0x7FD8, 3)]);
        assert_eq!(image(&file)[0x603E], 3);
        let file = rom(0x8000, &[(0x7FD8, 2)]);
        assert_eq!(image(&file)[0x603E], 0x0F);
    }

    #[test]
    fn copier_header_stays_in_the_programmed_image() {
        let mut file = vec![0xAB; 0x8200];
        file[0x200 + 0x7FD8] = 1;
        let out = image(&file);
        assert_menu(&out, &file);
        assert_eq!(out[0x603E], 0);
        assert_eq!(out[0x603F], 1);
    }

    #[test]
    fn hirom_sets_the_map_bit() {
        let file = rom(0x10000, &[(0xFFD5, 0x21), (0xFFD8, 1)]);
        let out = image(&file);
        assert_menu(&out, &file);
        assert_eq!(out[0x603C], 0);
        assert_eq!(out[0x603E], 0);
        assert_eq!(out[0x603F], 0x11);
    }

    #[test]
    fn sram_override_for_chip_0x15_is_32k() {
        let file = rom(0x8000, &[(0x7FD5, 0x20), (0x7FD6, 0x15)]);
        let out = image(&file);
        assert_eq!(out[0x603E], 0x0F);
        assert_eq!(out[0x603F], 1);
    }

    #[test]
    fn rom_slot_follows_the_client_limits() {
        assert_eq!(rom_slot(0x8_0000), 0);
        assert_eq!(rom_slot(0x8_0001), 1);
        assert_eq!(rom_slot(0x10_0000), 1);
        assert_eq!(rom_slot(0x10_0001), 3);
        assert_eq!(rom_slot(0x20_0000), 3);
        assert_eq!(rom_slot(0x20_0001), 7);
    }

    #[test]
    fn exhirom_and_type_4_bits() {
        let mut buf = vec![0u8; 0x10000];
        paint(&mut buf, 3, 0, 0x800);
        assert_eq!(buf[0x603F], 0x21);
        assert_eq!(buf[0x603E], 0);
        assert_eq!(buf[0xE03F], 0x21);

        let mut buf = vec![0u8; 0x10000];
        paint(&mut buf, 3, 0, 0);
        assert_eq!(buf[0x603F], 0x20);

        let mut buf = vec![0u8; 0x10000];
        paint(&mut buf, 4, 0xC0_0000, 0x800);
        assert_eq!(buf[0x603F], 1);
        assert_eq!(buf[0x603C], 7);
    }

    #[test]
    fn title_bytes_and_size_penalties_follow_the_signed_compares() {
        let base = header_score(&[0; 0x40], 0);
        let mut past_the_title = vec![0u8; 0x40];
        past_the_title[0x14] = 1;
        assert_eq!(header_score(&past_the_title, 0), base);
        let mut last_title = vec![0u8; 0x40];
        last_title[0x13] = 1;
        assert_eq!(header_score(&last_title, 0), base - 2);

        let mut size_6 = vec![0u8; 0x40];
        size_6[0x17] = 6;
        let mut size_13 = vec![0u8; 0x40];
        size_13[0x17] = 13;
        assert_eq!(header_score(&size_6, 0), header_score(&size_13, 0) + 2);

        let mut sram_7 = vec![0u8; 0x40];
        sram_7[0x18] = 7;
        let mut sram_28 = vec![0u8; 0x40];
        sram_28[0x18] = 28;
        assert_eq!(header_score(&sram_7, 0), header_score(&sram_28, 0));
        let mut sram_8 = vec![0u8; 0x40];
        sram_8[0x18] = 8;
        assert_eq!(header_score(&sram_8, 0), header_score(&sram_7, 0) - 2);
    }

    #[test]
    fn a_bank_above_0x7f_is_not_taken_from_inside_the_copy() {
        let mut rom = vec![0u8; 0x81 * 0x8000];
        rom[..0x10].fill(0x11);
        let bank128 = 0x80 * 0x8000;
        rom[bank128..bank128 + 0x10].fill(0x22);
        deinterleave(&mut rom, 256);
        assert_eq!(&rom[..0x10], &[0x11; 0x10]);
        assert_eq!(&rom[bank128..bank128 + 0x10], &[0x22; 0x10]);
    }

    #[test]
    fn preset_flags_match_the_early_returns() {
        assert_eq!(preset_flags(0x20, 0x15), Some((false, true, false)));
        assert_eq!(preset_flags(0x20, 0x13), Some((false, true, false)));
        assert_eq!(preset_flags(0x30, 0xF6), Some((false, false, true)));
        assert_eq!(preset_flags(0x21, 0x00), None);
        assert_eq!(preset_flags(0x00, 0x03), Some((false, false, false)));
    }
}
