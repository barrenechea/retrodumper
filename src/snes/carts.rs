//! Cart table walked by the client's info lookup (`0x472560`).
//!
//! `carts.json` is that walk: 3,814 rows, in order, with the title,
//! alternate title, ROM size, RAM size, mapper id, `$E000` block CRC, and
//! flags. `include_str!` compiles the file into this crate, so the running
//! program does not open it.
//! A repeated CRC resolves to the first row. `Cart::mapper` is the mapper
//! after the client's overrides, which run in this order: CRC `0x350BB992`
//! becomes 4, then `flags & 0x4000` becomes 5, `flags & 0x400` becomes 6,
//! and `flags & 0x4` becomes 7. A later override replaces an earlier one.

use std::collections::HashMap;
use std::sync::LazyLock;

use super::header::Header;
use super::mapper::Mapper;
use crate::{Error, Result};

/// Battery-backed SRAM.
pub const FLAG_BATTERY: u32 = 0x1;
/// CX4. Also forces mapper 7.
pub const FLAG_CX4: u32 = 0x4;
/// DSP enhancement.
pub const FLAG_DSP: u32 = 0x8;
/// Real-time clock.
pub const FLAG_RTC: u32 = 0x100;
/// SA-1. The row's mapper id stays LoROM.
pub const FLAG_SA1: u32 = 0x200;
/// SDD-1. Also forces mapper 6.
pub const FLAG_SDD1: u32 = 0x400;
/// Super FX. The row's mapper id stays LoROM.
pub const FLAG_SUPER_FX: u32 = 0x800;
/// SPC7110. Also forces mapper 5.
pub const FLAG_SPC7110: u32 = 0x4000;

/// Derby Stallion 96. The row says LoROM; the lookup forces mapper 4.
pub const DERBY_STALLION_96: u32 = 0x350B_B992;

#[derive(Debug)]
pub struct Cart {
    pub crc: u32,
    pub title: String,
    pub alt: String,
    pub rom_size: usize,
    pub ram_size: usize,
    /// Mapper id stored at row offset `0x10`, before overrides.
    pub mmc: u32,
    pub flags: u32,
    /// Mapper the client selects for a dump.
    pub mapper: Mapper,
}

struct Table {
    rows: Vec<Cart>,
    first: HashMap<u32, usize>,
}

fn table() -> &'static Table {
    static TABLE: LazyLock<Table> = LazyLock::new(|| parse(include_str!("carts.json")));
    &TABLE
}

/// Names the client prints for enhancement bits, client lines first.
pub fn enhancements(flags: u32) -> Vec<&'static str> {
    let mut names = Vec::new();
    if flags & FLAG_SDD1 != 0 {
        names.push("SDD-1");
    }
    if flags & FLAG_SPC7110 != 0 {
        names.push("SPC7110");
    }
    if flags & FLAG_CX4 != 0 {
        names.push("C4");
    }
    if flags & FLAG_BATTERY != 0 {
        names.push("battery");
    }
    if flags & FLAG_DSP != 0 {
        names.push("DSP");
    }
    if flags & FLAG_RTC != 0 {
        names.push("RTC");
    }
    if flags & FLAG_SA1 != 0 {
        names.push("SA-1");
    }
    if flags & FLAG_SUPER_FX != 0 {
        names.push("Super FX");
    }
    names
}

#[derive(Debug)]
pub struct Selection {
    pub title: String,
    pub alt: String,
    pub mapper: Mapper,
    pub rom_size: usize,
    pub ram_size: usize,
    pub flags: u32,
    /// False for the Homebrew signature path, which is not a table row.
    pub from_database: bool,
}

/// Table row for this block CRC, or the Homebrew signature when the CRC misses.
pub fn identify(h: &Header) -> Option<Selection> {
    if let Some(c) = lookup(h.block_crc) {
        return Some(Selection {
            title: c.title.clone(),
            alt: c.alt.clone(),
            mapper: c.mapper,
            rom_size: c.rom_size,
            ram_size: c.ram_size,
            flags: c.flags,
            from_database: true,
        });
    }
    if h.homebrew {
        return Some(Selection {
            title: h.title.clone(),
            alt: String::new(),
            mapper: Mapper::Homebrew,
            rom_size: h.rom_size(),
            ram_size: h.sram_size(),
            flags: 0,
            from_database: false,
        });
    }
    None
}

/// Dump mapper and ROM size. A table miss needs both `--mmc` and `--size`.
pub fn choose_dump(h: &Header, mmc: i64, size: i64) -> Result<IoChoice> {
    choose(
        h,
        mmc,
        size,
        |s| s.rom_size,
        "--mmc and --size",
        "unknown ROM size; use --size",
    )
}

/// Save mapper and RAM size. A table miss needs both `--mmc` and `--size`.
pub fn choose_read_save(h: &Header, mmc: i64, size: i64) -> Result<IoChoice> {
    choose(
        h,
        mmc,
        size,
        |s| s.ram_size,
        "--mmc and --size",
        "no SRAM size known; use --size",
    )
}

/// Mapper for a save write. A table miss needs `--mmc`. The file supplies the length.
pub fn choose_mapper(h: &Header, mmc: i64) -> Result<Mapper> {
    if let Some(mapper) = mapper_override(mmc)? {
        return Ok(mapper);
    }
    identify(h)
        .map(|s| s.mapper)
        .ok_or_else(|| undetected(h.block_crc, "--mmc"))
}

/// RAM size from a table hit. Zero means the write is not capped.
pub fn ram_cap(h: &Header) -> usize {
    identify(h).map(|s| s.ram_size).unwrap_or(0)
}

#[derive(Debug)]
pub struct IoChoice {
    pub title: String,
    pub mapper: Mapper,
    pub size: usize,
}

fn choose(
    h: &Header,
    mmc: i64,
    size: i64,
    from_row: impl Fn(&Selection) -> usize,
    miss: &str,
    zero: &str,
) -> Result<IoChoice> {
    let mapper = mapper_override(mmc)?;
    let manual = size_override(size)?;
    if let Some(sel) = identify(h) {
        let n = manual.unwrap_or_else(|| from_row(&sel));
        if n == 0 {
            return Err(Error::Cli(zero.into()));
        }
        return Ok(IoChoice {
            title: sel.title,
            mapper: mapper.unwrap_or(sel.mapper),
            size: n,
        });
    }
    match (mapper, manual) {
        (Some(mapper), Some(n)) => Ok(IoChoice {
            title: h.title.clone(),
            mapper,
            size: n,
        }),
        _ => Err(undetected(h.block_crc, miss)),
    }
}

fn mapper_override(mmc: i64) -> Result<Option<Mapper>> {
    if mmc == 0 {
        return Ok(None);
    }
    let n = u32::try_from(mmc)
        .map_err(|_| Error::Cli(format!("invalid mapper {mmc} (must be 1-8)")))?;
    Mapper::from_u32(n)
        .map(Some)
        .ok_or_else(|| Error::Cli(format!("invalid mapper {mmc} (must be 1-8)")))
}

fn size_override(size: i64) -> Result<Option<usize>> {
    if size == 0 {
        return Ok(None);
    }
    usize::try_from(size)
        .map(Some)
        .map_err(|_| Error::Cli(format!("invalid --size {size}")))
}

fn undetected(crc: u32, need: &str) -> Error {
    Error::Cli(format!(
        "Can not detect cartridge detail information. Error code: 0x{crc:08X}. Pass {need}."
    ))
}

/// First row for this block CRC, after mapper overrides.
pub fn lookup(crc: u32) -> Option<&'static Cart> {
    let t = table();
    t.first.get(&crc).map(|&i| &t.rows[i])
}

#[cfg(test)]
fn rows() -> &'static [Cart] {
    &table().rows
}

/// Mapper id left after the overrides. `None` when that id is not 1–8.
fn apply_overrides(crc: u32, mmc: u32, flags: u32) -> Option<Mapper> {
    let mut m = mmc;
    if crc == DERBY_STALLION_96 {
        m = Mapper::Derby96.number();
    }
    if flags & FLAG_SPC7110 != 0 {
        m = Mapper::SPC7110.number();
    }
    if flags & FLAG_SDD1 != 0 {
        m = Mapper::SDD1.number();
    }
    if flags & FLAG_CX4 != 0 {
        m = Mapper::CX4.number();
    }
    Mapper::from_u32(m)
}

fn parse(text: &str) -> Table {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
    };
    p.eat(b'[');
    let mut rows = Vec::new();
    let mut first = HashMap::new();
    loop {
        p.skip();
        if p.peek() == Some(b']') {
            p.i += 1;
            break;
        }
        if !rows.is_empty() {
            p.eat(b',');
        }
        let raw = p.object();
        let mapper = apply_overrides(raw.crc, raw.mmc, raw.flags)
            .unwrap_or_else(|| panic!("sfc cart mapper: crc {:#x} mmc {}", raw.crc, raw.mmc));
        let index = rows.len();
        first.entry(raw.crc).or_insert(index);
        rows.push(Cart {
            crc: raw.crc,
            title: raw.title,
            alt: raw.alt,
            rom_size: raw.rom as usize,
            ram_size: raw.ram as usize,
            mmc: raw.mmc,
            flags: raw.flags,
            mapper,
        });
    }
    p.skip();
    assert_eq!(p.i, p.s.len(), "sfc cart table trailing bytes");
    Table { rows, first }
}

struct Raw {
    title: String,
    alt: String,
    rom: u32,
    ram: u32,
    mmc: u32,
    crc: u32,
    flags: u32,
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn object(&mut self) -> Raw {
        self.eat(b'{');
        let mut title = None;
        let mut alt = None;
        let mut rom = None;
        let mut ram = None;
        let mut mmc = None;
        let mut crc = None;
        let mut flags = None;
        let mut any = false;
        loop {
            self.skip();
            if self.peek() == Some(b'}') {
                self.i += 1;
                break;
            }
            if any {
                self.eat(b',');
            }
            any = true;
            let key = self.string();
            self.eat(b':');
            match key.as_str() {
                "title" => title = Some(self.string()),
                "alt" => alt = Some(self.string()),
                "rom" => rom = Some(self.number()),
                "ram" => ram = Some(self.number()),
                "mmc" => mmc = Some(self.number()),
                "crc" => crc = Some(self.number()),
                "flags" => flags = Some(self.number()),
                other => panic!("sfc cart table field {other:?}"),
            }
        }
        Raw {
            title: title.expect("sfc cart title"),
            alt: alt.expect("sfc cart alt"),
            rom: rom.expect("sfc cart rom"),
            ram: ram.expect("sfc cart ram"),
            mmc: mmc.expect("sfc cart mmc"),
            crc: crc.expect("sfc cart crc"),
            flags: flags.expect("sfc cart flags"),
        }
    }

    fn string(&mut self) -> String {
        self.skip();
        self.eat(b'"');
        let mut raw = Vec::new();
        loop {
            let b = self.bump();
            match b {
                b'"' => break,
                b'\\' => {
                    let ch = match self.bump() {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.hex4(),
                        _ => panic!("sfc cart table bad escape"),
                    };
                    raw.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                }
                c => raw.push(c),
            }
        }
        String::from_utf8(raw).expect("sfc cart table utf-8")
    }

    fn hex4(&mut self) -> char {
        let mut n = 0u32;
        for _ in 0..4 {
            n = (n << 4) | u32::from(hex_val(self.bump()));
        }
        char::from_u32(n).unwrap_or_else(|| panic!("sfc cart table unicode: {n:#x}"))
    }

    fn number(&mut self) -> u32 {
        self.skip();
        let start = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        assert!(self.i > start, "sfc cart table number: no digits");
        let text =
            std::str::from_utf8(&self.s[start..self.i]).expect("sfc cart table number: digit text");
        text.parse()
            .unwrap_or_else(|_| panic!("sfc cart table number: u32 parse {text}"))
    }

    fn eat(&mut self, want: u8) {
        self.skip();
        let got = self.bump();
        assert_eq!(got, want, "sfc cart table");
    }

    fn bump(&mut self) -> u8 {
        let b = self.peek().expect("sfc cart table truncated");
        self.i += 1;
        b
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn skip(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.i += 1;
        }
    }
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => panic!("sfc cart table hex"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snes::header::Header;

    #[test]
    fn table_covers_the_client_walk() {
        let rows = rows();
        assert_eq!(rows.len(), 3814);
        assert_eq!(
            rows.last().map(|c| c.title.as_str()),
            Some("Zootto Mahjong! (Japan) (Rev 1) (NP)")
        );
        let mut homebrew = 0;
        for c in rows {
            if c.mmc == 8 {
                assert_eq!(c.title, "Tengu Hombrew V1 Simple");
                assert_eq!(c.flags, 0);
                assert_eq!(c.mapper, Mapper::Homebrew);
                homebrew += 1;
            } else {
                assert!(matches!(c.mmc, 1..=3), "{}", c.title);
            }
        }
        assert_eq!(homebrew, 1);
    }

    #[test]
    fn derby_row_is_lorom_and_the_override_selects_mapper_4() {
        let c = lookup(DERBY_STALLION_96).unwrap();
        assert_eq!(c.title, "Derby Stallion 96 (Japan)");
        assert_eq!(c.alt, "ダービースタリオン96");
        assert_eq!(c.mmc, Mapper::LoRom.number());
        assert_eq!(c.mapper, Mapper::Derby96);
        assert_eq!(c.rom_size, 0x30_0000);
        assert_eq!(c.ram_size, 0x8000);
        assert_eq!(c.flags, 0);
    }

    #[test]
    fn donkey_kong_country_rev2_is_the_first_row_for_its_crc() {
        let c = lookup(0xA248_E81A).unwrap();
        assert_eq!(rows().iter().filter(|r| r.crc == 0xA248_E81A).count(), 2);
        assert_eq!(c.title, "Donkey Kong Country (USA) (Rev 2)");
        assert_eq!(c.alt, "");
        assert_eq!(c.mmc, Mapper::HiRom.number());
        assert_eq!(c.mapper, Mapper::HiRom);
        assert_eq!(c.rom_size, 0x40_0000);
        assert_eq!(c.ram_size, 0x800);
    }

    #[test]
    fn enhancement_flags_replace_the_row_mapper() {
        let cx4 = lookup(0xE918_88D3).unwrap();
        assert_eq!(cx4.title, "Mega Man X2 (Europe)");
        assert_eq!(cx4.mmc, Mapper::LoRom.number());
        assert_eq!(cx4.flags, 0x4);
        assert_eq!(cx4.mapper, Mapper::CX4);

        let sdd = lookup(0x74D8_DFAB).unwrap();
        assert_eq!(sdd.title, "Star Ocean (Japan)");
        assert_eq!(sdd.flags, 0x401);
        assert_eq!(sdd.mapper, Mapper::SDD1);

        let spc = lookup(0x23E4_EA2F).unwrap();
        assert_eq!(spc.title, "Momotarou Dentetsu Happy (Japan)");
        assert_eq!(spc.flags, 0x4001);
        assert_eq!(spc.mapper, Mapper::SPC7110);
    }

    #[test]
    fn a_repeated_crc_keeps_the_earlier_row() {
        let c = lookup(0xD8F4_9994).unwrap();
        assert_eq!(c.title, "All data are 0x00");
        assert_eq!(rows().iter().filter(|r| r.crc == 0xD8F4_9994).count(), 2);
    }

    #[test]
    fn json_strings_keep_escapes() {
        let t =
            parse(r#"[{"title":"a\"b","alt":"\u30c0","rom":1,"ram":0,"mmc":1,"crc":9,"flags":0}]"#);
        assert_eq!(t.rows[0].title, "a\"b");
        assert_eq!(t.rows[0].alt, "ダ");
        assert_eq!(t.rows[0].mapper, Mapper::LoRom);
    }

    #[test]
    fn an_unknown_crc_is_a_miss() {
        assert!(lookup(0x0000_0001).is_none());
    }

    #[test]
    fn overrides_are_applied_in_client_order() {
        assert_eq!(apply_overrides(0, 1, 0), Some(Mapper::LoRom));
        assert_eq!(
            apply_overrides(DERBY_STALLION_96, 1, 0),
            Some(Mapper::Derby96)
        );
        assert_eq!(apply_overrides(0, 1, FLAG_SPC7110), Some(Mapper::SPC7110));
        assert_eq!(apply_overrides(0, 2, FLAG_SDD1), Some(Mapper::SDD1));
        assert_eq!(apply_overrides(0, 1, FLAG_CX4), Some(Mapper::CX4));
        assert_eq!(
            apply_overrides(DERBY_STALLION_96, 1, FLAG_SPC7110 | FLAG_SDD1 | FLAG_CX4),
            Some(Mapper::CX4)
        );
        assert_eq!(apply_overrides(0, 8, 0), Some(Mapper::Homebrew));
        assert_eq!(apply_overrides(0, 0, 0), None);
        assert_eq!(apply_overrides(0, 9, 0), None);
    }

    #[test]
    fn enhancements_follow_the_client_lines() {
        assert_eq!(enhancements(0x4), ["C4"]);
        assert_eq!(enhancements(0x401), ["SDD-1", "battery"]);
        assert_eq!(enhancements(0x4001), ["SPC7110", "battery"]);
        assert_eq!(enhancements(FLAG_SA1 | FLAG_SUPER_FX), ["SA-1", "Super FX"]);
    }

    #[test]
    fn a_table_hit_wins_over_the_homebrew_signature() {
        let h = Header {
            block_crc: DERBY_STALLION_96,
            homebrew: true,
            ..Default::default()
        };
        let s = identify(&h).unwrap();
        assert!(s.from_database);
        assert_eq!(s.title, "Derby Stallion 96 (Japan)");
        assert_eq!(s.alt, "ダービースタリオン96");
        assert_eq!(s.mapper, Mapper::Derby96);
        assert_eq!(s.rom_size, 0x30_0000);
        assert_eq!(s.ram_size, 0x8000);
    }

    #[test]
    fn a_signature_miss_uses_the_homebrew_sizes() {
        let h = Header {
            homebrew: true,
            title: "GAME".into(),
            rom_override: 0x60_0000,
            sram_override: 0x8000,
            ..Default::default()
        };
        let s = identify(&h).unwrap();
        assert!(!s.from_database);
        assert_eq!(s.title, "GAME");
        assert_eq!(s.mapper, Mapper::Homebrew);
        assert_eq!(s.rom_size, 0x60_0000);
        assert_eq!(s.ram_size, 0x8000);
    }

    #[test]
    fn an_unknown_cart_is_not_dumped_from_the_header() {
        let h = Header {
            block_crc: 1,
            map_mode: 0x21,
            title: "HEADER".into(),
            ..Default::default()
        };
        assert!(identify(&h).is_none());
        let err = choose_dump(&h, 0, 0).unwrap_err().to_string();
        assert!(err.contains("Can not detect cartridge detail information"));
        assert!(err.contains("Error code: 0x00000001"));
        assert!(err.contains("Pass --mmc and --size"));
        assert!(choose_dump(&h, 2, 0).is_err());
        assert!(choose_dump(&h, 0, 4096).is_err());
        let c = choose_dump(&h, 2, 4096).unwrap();
        assert_eq!(c.mapper, Mapper::HiRom);
        assert_eq!(c.size, 4096);
        assert_eq!(c.title, "HEADER");
    }

    #[test]
    fn a_known_cart_uses_the_row_unless_flags_override_it() {
        let h = Header {
            block_crc: DERBY_STALLION_96,
            ..Default::default()
        };
        let c = choose_dump(&h, 0, 0).unwrap();
        assert_eq!(c.mapper, Mapper::Derby96);
        assert_eq!(c.size, 0x30_0000);
        let forced = choose_dump(&h, 1, 0).unwrap();
        assert_eq!(forced.mapper, Mapper::LoRom);
        assert_eq!(forced.size, 0x30_0000);
        let sized = choose_dump(&h, 0, 0x8000).unwrap();
        assert_eq!(sized.mapper, Mapper::Derby96);
        assert_eq!(sized.size, 0x8000);
    }

    #[test]
    fn save_read_uses_ram_size_and_a_miss_needs_both_flags() {
        let derby = Header {
            block_crc: DERBY_STALLION_96,
            ..Default::default()
        };
        let c = choose_read_save(&derby, 0, 0).unwrap();
        assert_eq!(c.size, 0x8000);
        assert_eq!(c.mapper, Mapper::Derby96);
        assert_eq!(ram_cap(&derby), 0x8000);
        assert_eq!(choose_mapper(&derby, 0).unwrap(), Mapper::Derby96);

        let tengu = Header {
            block_crc: 0x0997_0FD0,
            ..Default::default()
        };
        assert!(choose_read_save(&tengu, 0, 0).is_err());
        let manual = choose_read_save(&tengu, 0, 0x100).unwrap();
        assert_eq!(manual.mapper, Mapper::Homebrew);
        assert_eq!(manual.size, 0x100);

        let unknown = Header {
            block_crc: 1,
            ..Default::default()
        };
        assert!(choose_read_save(&unknown, 0, 0).is_err());
        assert!(choose_mapper(&unknown, 0).is_err());
        assert_eq!(choose_mapper(&unknown, 3).unwrap(), Mapper::ExHiRom);
        assert_eq!(ram_cap(&unknown), 0);
    }
}
