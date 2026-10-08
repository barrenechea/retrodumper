//! Command `retrodump` is a cross-platform replacement for the Windows-only
//! MultiDumper client, starting with the SNES/SFC dumper on the Retro Base
//! (USB mass-storage transport).

use std::io::Write;
use std::process::ExitCode;

use retrodump::Error;
use retrodump::device::Device;
use retrodump::snes::{self, SFC};

const CRC32: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("retrodump: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), Error> {
    if args.is_empty() {
        return usage(None);
    }
    match args[0].as_str() {
        "device" => cmd_device(&args[1..]),
        "snes" => cmd_snes(&args[1..]),
        "debug" => cmd_debug(&args[1..]),
        "help" | "-h" | "--help" => usage(None),
        other => usage(Some(Error::Cli(format!("unknown command {other:?}")))),
    }
}

fn usage(err: Option<Error>) -> Result<(), Error> {
    if let Some(e) = &err {
        eprintln!("{e}");
    }
    eprintln!(
        "Usage:
  retrodump device                  list the attached Retro Dumper
  retrodump snes info               read and parse the cartridge header
  retrodump snes dump [-o FILE]     dump the ROM
  retrodump snes read-save [-o FILE]
  retrodump snes write-save FILE
  retrodump snes flash FILE         program a flash cart
  retrodump debug peek ADDR LEN     hexdump bus addresses via DUMP.ROM
  retrodump help"
    );
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn get_flag(args: &[String], name: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let stripped = a.trim_start_matches('-');
        if let Some(eq) = stripped.find('=') {
            if &stripped[..eq] == name {
                return Some(stripped[eq + 1..].to_string());
            }
        } else if stripped == name && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| {
        let s = a.trim_start_matches('-');
        s == name || s.starts_with(&format!("{name}="))
    })
}

fn get_int(args: &[String], name: &str) -> Result<i64, Error> {
    match get_flag(args, name) {
        None => Ok(0),
        Some(v) => parse_c_int(&v).ok_or_else(|| Error::Cli(format!("invalid --{name} {v:?}"))),
    }
}

/// `strconv.ParseInt(s, 0, 64)`: optional sign, then `0x` hex, `0b` binary,
/// a leading `0` for octal, otherwise decimal.
fn parse_c_int(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.is_empty() {
        return None;
    }
    let (sign, b) = match b[0] {
        b'+' => (1i64, &b[1..]),
        b'-' => (-1, &b[1..]),
        _ => (1, b),
    };
    if b.is_empty() {
        return None;
    }
    let (radix, digits) = if b.len() >= 2 && b[0] == b'0' && (b[1] == b'x' || b[1] == b'X') {
        (16u32, &b[2..])
    } else if b.len() >= 2 && b[0] == b'0' && (b[1] == b'b' || b[1] == b'B') {
        (2, &b[2..])
    } else if b.len() >= 2 && b[0] == b'0' {
        (8, &b[1..])
    } else {
        (10, b)
    };
    if digits.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    for &c in digits {
        let d = match c {
            b'0'..=b'9' => u32::from(c - b'0'),
            b'a'..=b'z' => u32::from(c - b'a') + 10,
            b'A'..=b'Z' => u32::from(c - b'A') + 10,
            _ => return None,
        };
        if d >= radix {
            return None;
        }
        n = n.checked_mul(i64::from(radix))?.checked_add(i64::from(d))?;
    }
    n.checked_mul(sign)
}

fn first_positional(args: &[String]) -> Option<String> {
    args.iter().find(|a| !a.starts_with('-')).cloned()
}

fn open_device() -> Result<Device, Error> {
    Device::find()
}

fn cmd_device(_args: &[String]) -> Result<(), Error> {
    let dev = open_device()?;
    println!("root:     {}", dev.root.display());
    println!("name:     {}", dev.info.name);
    if !dev.info.second.is_empty() {
        println!("second:   {}", dev.info.second);
    }
    println!("id0/id1:  0x{:08X} 0x{:08X}", dev.info.id0, dev.info.id1);
    println!("version:  {}", dev.info.version);
    if dev.info.legacy {
        println!("format:   legacy (2020 firmware)");
    }
    let root = dev.root.clone();
    let entries =
        std::fs::read_dir(&root).map_err(|e| Error::Cli(format!("list device root: {e}")))?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        println!("  {:<16} {:>10}", name, size);
    }
    Ok(())
}

fn cmd_snes(args: &[String]) -> Result<(), Error> {
    if args.is_empty() {
        return Err(Error::Cli(
            "snes: missing subcommand (info, dump, read-save, write-save, flash)".into(),
        ));
    }
    match args[0].as_str() {
        "info" => cmd_snes_info(&args[1..]),
        "dump" => cmd_snes_dump(&args[1..]),
        "read-save" => cmd_snes_read_save(&args[1..]),
        "write-save" => cmd_snes_write_save(&args[1..]),
        "flash" => cmd_snes_flash(&args[1..]),
        other => Err(Error::Cli(format!("snes: unknown subcommand {other:?}"))),
    }
}

fn cmd_snes_info(args: &[String]) -> Result<(), Error> {
    if !args.is_empty() {
        return Err(Error::Cli(format!(
            "snes info: unexpected arguments: {args:?}"
        )));
    }
    let dev = open_device()?;
    let mut sfc = SFC::new(Box::new(dev));
    let h = sfc.info()?;
    if let Some(s) = snes::carts::identify(&h) {
        println!("title:     {}", s.title);
        if !s.alt.is_empty() {
            println!("alt:       {}", s.alt);
        }
        println!("mapper:    {} ({})", s.mapper.number(), s.mapper.name());
        println!(
            "rom size:  {} bytes ({} Mbit)",
            s.rom_size,
            s.rom_size >> 17
        );
        println!("sram size: {} bytes ({} Kbit)", s.ram_size, s.ram_size >> 7);
        for name in snes::carts::enhancements(s.flags) {
            println!("enhancement: {name}");
        }
        if s.from_database && s.title != h.title {
            println!("header:    {}", h.title);
        }
    } else {
        let hint = snes::detect_mapper(&h);
        println!("Can not detect cartridge detail information.");
        println!("Error code: 0x{:08X}", h.block_crc);
        println!("header:    {}", h.title);
        println!("hint:      {} ({})", hint.number(), hint.name());
        println!(
            "hint rom:  {} bytes (code 0x{:02X})",
            h.rom_size(),
            h.size_code
        );
        println!(
            "hint sram: {} bytes (code 0x{:02X})",
            h.sram_size(),
            h.sram_code
        );
        if h.is_sfx() {
            println!("note:      Super FX");
        }
        if h.is_sa1() {
            println!("note:      SA-1");
        }
    }
    println!("map mode:  0x{:02X}", h.map_mode);
    println!("chip:      0x{:02X}", h.chip);
    if h.exp_ram_size() > 0 {
        println!("exp ram:   {} bytes", h.exp_ram_size());
    }
    println!("region:    0x{:02X}", h.region);
    println!("version:   1.{}", h.version);
    let valid = if h.valid() { "valid" } else { "INVALID" };
    println!(
        "checksum:  0x{:04X} (complement 0x{:04X}, header {})",
        h.checksum, h.complement, valid
    );
    println!("license:   0x{:02X}", h.license);
    if h.license == 0x33 {
        println!("maker:     {}", String::from_utf8_lossy(&h.maker_code));
        println!("game code: {}", String::from_utf8_lossy(&h.game_code));
    }
    println!("block crc: 0x{:08X}", h.block_crc);
    Ok(())
}

fn cmd_snes_dump(args: &[String]) -> Result<(), Error> {
    let out = get_flag(args, "o").unwrap_or_default();
    let size = get_int(args, "size")?;
    let mmc = get_int(args, "mmc")?;
    let no_verify = has_flag(args, "no-verify");

    let dev = open_device()?;
    let mut sfc = SFC::new(Box::new(dev));
    let h = sfc.info()?;
    let choice = snes::carts::choose_dump(&h, mmc, size)?;
    eprintln!(
        "dumping {} ({}), {} bytes",
        choice.title,
        choice.mapper.name(),
        choice.size
    );

    let mut rom = Vec::with_capacity(choice.size);
    {
        let mut dumper = snes::Dumper::new(&mut sfc, choice.mapper, choice.size);
        let last = &mut -1i64;
        dumper.dump(&mut rom, |done, total| percent(done, total, last))?;
    }
    eprintln!();

    let patched = snes::apply_known_bad_cart_patch(&mut rom);
    if patched {
        eprintln!("note: applied known-bad-cart patch (byte 0xB06BFB = 6)");
    }
    // Report the CRC32 of the dumped ROM, matching the C client, which
    // substitutes 0x349D7025 for the known-bad cart.
    let reported_crc = if patched {
        0x349D7025
    } else {
        CRC32.checksum(&rom)
    };
    eprintln!("crc32:     0x{:08X}", reported_crc);

    let mut checksum_ok = true;
    if no_verify {
        eprintln!("verification skipped");
    } else {
        let (ok, got) = snes::verify_checksum(&rom, h.checksum);
        if ok {
            eprintln!("checksum OK (0x{:04X})", got);
        } else {
            checksum_ok = false;
            eprintln!(
                "checksum FAILED: got 0x{:04X}, header wants 0x{:04X}",
                got, h.checksum
            );
            for ts in snes::trimmed_sizes(choice.size) {
                if snes::verify_checksum(&rom[..ts], h.checksum).0 {
                    eprintln!("note: checksum passes at trimmed size {ts} bytes");
                    break;
                }
            }
        }
    }

    let path = if out.is_empty() {
        sanitize_title(&choice.title) + ".sfc"
    } else {
        out
    };
    std::fs::write(&path, &rom).map_err(Error::Io)?;
    eprintln!("wrote {} ({} bytes)", path, rom.len());
    // A failed checksum is a real failure for a dumper: exit non-zero so
    // scripts/pipelines can detect it. The file is still written (with the
    // trimmed-size hint above) so a good image isn't lost.
    if !checksum_ok {
        return Err(Error::Cli(format!(
            "checksum verification failed (file written to {path}; use --no-verify to keep it)"
        )));
    }
    Ok(())
}

fn cmd_snes_read_save(args: &[String]) -> Result<(), Error> {
    let out = get_flag(args, "o").unwrap_or_default();
    let size = get_int(args, "size")?;
    let mmc = get_int(args, "mmc")?;

    let dev = open_device()?;
    let mut sfc = SFC::new(Box::new(dev));
    let h = sfc.info()?;
    let choice = snes::carts::choose_read_save(&h, mmc, size)?;
    let data = sfc.read_save(choice.mapper, choice.size)?;
    let path = if out.is_empty() {
        sanitize_title(&choice.title) + ".srm"
    } else {
        out
    };
    std::fs::write(&path, &data).map_err(Error::Io)?;
    eprintln!("wrote {} ({} bytes)", path, data.len());
    Ok(())
}

fn cmd_snes_flash(args: &[String]) -> Result<(), Error> {
    let mmc = get_int(args, "mmc")?;
    let no_verify = has_flag(args, "no-verify");
    let file = positionals(args)
        .into_iter()
        .next()
        .ok_or_else(|| Error::Cli("usage: retrodump snes flash FILE".into()))?;
    let image = std::fs::read(&file).map_err(Error::Io)?;

    let dev = open_device()?;
    let mut sfc = SFC::new(Box::new(dev));
    let mapper = flash_mapper(&mut sfc, mmc)?;
    eprintln!("flashing {} bytes ({})", image.len(), mapper.name());
    let written = {
        let mut flash = snes::Flash::new(&mut sfc, mapper);
        let mut last = -1i64;
        retrodump::cart::program_image(
            &mut flash,
            &image,
            |n| eprintln!("flash: {n} bytes"),
            || eprintln!("erasing..."),
            || std::thread::sleep(std::time::Duration::from_secs(1)),
            |done, total| percent(done, total, &mut last),
        )?
    };
    eprintln!();
    if no_verify {
        eprintln!("verification skipped");
    } else {
        let mut got = Vec::new();
        let mut last = -1i64;
        snes::Dumper::new(&mut sfc, mapper, written.len()).dump(&mut got, |done, total| {
            percent(done, total, &mut last);
        })?;
        eprintln!();
        if got != written {
            return Err(Error::FlashVerifyFailed);
        }
        eprintln!("verified by read-back");
    }
    eprintln!("wrote {} bytes to flash", written.len());
    Ok(())
}

fn flash_mapper(sfc: &mut SFC, mmc: i64) -> Result<snes::Mapper, Error> {
    match sfc.info() {
        Ok(h) => snes::carts::choose_mapper(&h, mmc),
        Err(Error::NoCartridge) => match u32::try_from(mmc).ok().and_then(snes::Mapper::from_u32) {
            Some(mapper) => Ok(mapper),
            None if mmc == 0 => Err(Error::Cli("no cartridge header; pass --mmc".into())),
            None => Err(Error::Cli(format!("invalid mapper {mmc} (must be 1-8)"))),
        },
        Err(e) => Err(e),
    }
}

fn percent(done: usize, total: usize, last: &mut i64) {
    if total == 0 {
        return;
    }
    let pct = done as i64 * 100 / total as i64;
    if pct / 10 != *last / 10 {
        *last = pct;
        eprint!("\r  {pct:3}%");
    }
}

fn positionals(args: &[String]) -> Vec<String> {
    const VALUED: &[&str] = &["o", "size", "mmc"];
    let mut out = Vec::new();
    let mut skip_value = false;
    for a in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        if let Some(name) = a.strip_prefix('-') {
            let name = name.trim_start_matches('-');
            let (name, has_value) = match name.split_once('=') {
                Some((n, _)) => (n, true),
                None => (name, false),
            };
            if !has_value && VALUED.contains(&name) {
                skip_value = true;
            }
            continue;
        }
        out.push(a.clone());
    }
    out
}

fn cmd_snes_write_save(args: &[String]) -> Result<(), Error> {
    let mmc = get_int(args, "mmc")?;
    let no_verify = has_flag(args, "no-verify");
    let file = first_positional(args)
        .ok_or_else(|| Error::Cli("usage: retrodump snes write-save FILE".into()))?;

    let data = std::fs::read(&file).map_err(Error::Io)?;
    let dev = open_device()?;
    let mut sfc = SFC::new(Box::new(dev));
    let h = sfc.info()?;
    let mapper = snes::carts::choose_mapper(&h, mmc)?;
    let cap = snes::carts::ram_cap(&h);
    if cap > 0 && data.len() > cap {
        return Err(Error::Cli(format!(
            "save is {} bytes but SRAM size is {cap}",
            data.len()
        )));
    }
    sfc.write_save(mapper, &data, !no_verify)?;
    let note = if no_verify {
        ""
    } else {
        " (verified by read-back)"
    };
    eprintln!("wrote {} bytes to cartridge SRAM{note}", data.len());
    Ok(())
}

fn cmd_debug(args: &[String]) -> Result<(), Error> {
    if args.len() != 3 || args[0] != "peek" {
        return Err(Error::Cli("usage: retrodump debug peek ADDR LEN".into()));
    }
    let addr = parse_c_int(&args[1])
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| Error::Cli(format!("debug: bad address: {}", args[1])))?;
    let n = parse_c_int(&args[2])
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| Error::Cli(format!("debug: bad length: {}", args[2])))?;
    let dev = open_device()?;
    let mut sfc = SFC::new(Box::new(dev));
    sfc.init()?;
    let b = sfc.read_bus(addr, n)?;
    let mut stdout = std::io::stdout();
    hexdump(&mut stdout, addr, &b)?;
    Ok(())
}

fn hexdump(w: &mut impl Write, base: usize, b: &[u8]) -> std::io::Result<()> {
    const WIDTH: usize = 16;
    for off in (0..b.len()).step_by(WIDTH) {
        let end = (off + WIDTH).min(b.len());
        let chunk = &b[off..end];
        let mut parts: Vec<String> = Vec::new();
        let mut asc = String::new();
        for i in 0..WIDTH {
            if i < chunk.len() {
                parts.push(format!("{:02x}", chunk[i]));
                asc.push(if (0x20..=0x7E).contains(&chunk[i]) {
                    chunk[i] as char
                } else {
                    '.'
                });
            } else {
                parts.push("  ".into());
                asc.push(' ');
            }
        }
        writeln!(w, "{:06X}  {} {}", base + off, parts.join(" "), asc)?;
    }
    Ok(())
}

fn sanitize_title(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|r| {
            if r.is_ascii_alphanumeric() || r == '-' || r == '_' {
                r
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() { "rom".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::parse_c_int;

    #[test]
    fn parse_c_int_matches_base_zero() {
        assert_eq!(parse_c_int("255"), Some(255));
        assert_eq!(parse_c_int("0xFFC0"), Some(0xFFC0));
        assert_eq!(parse_c_int("0X10"), Some(0x10));
        assert_eq!(parse_c_int("0b1010"), Some(0b1010));
        assert_eq!(parse_c_int("010"), Some(8));
        assert_eq!(parse_c_int("0"), Some(0));
        assert_eq!(parse_c_int("-2"), Some(-2));
        assert_eq!(parse_c_int("0x"), None);
        assert_eq!(parse_c_int("08"), None);
    }

    #[test]
    fn positionals_skip_flag_values() {
        let args = ["--mmc", "1", "game.sfc", "--no-verify"]
            .map(str::to_string)
            .to_vec();
        assert_eq!(super::positionals(&args), vec!["game.sfc".to_string()]);
        let args = ["game.sfc", "--mmc=2"].map(str::to_string).to_vec();
        assert_eq!(super::positionals(&args), vec!["game.sfc".to_string()]);
    }
}
