//! macOS (15+) blocks connections to the local network unless the process's
//! "responsible" app has the Local Network permission. A process started
//! inside a terminal daemon has no responsible app that can ever be granted
//! it: the daemon detaches from the terminal app that launched it, so a bus
//! dialing its hub over the LAN fails with "no route to host" and macOS
//! shows no alert.
//!
//! The fix has two halves. The build (make macos) embeds an Info.plist in the
//! binary and signs it with an Apple-issued identity, which lets the binary
//! hold the permission itself; macOS keys the grant on the bundle identifier,
//! so rebuilds keep it. And the bus re-spawns itself with responsibility
//! disclaimed (spawn_disclaimed), so macOS asks about this binary instead of
//! whatever the daemon's chain resolves to. See TN3179.

/// This build can't spawn disclaimed processes: it isn't a darwin build.
/// (Go: `ErrUnsupported`, "disclaimed spawn needs a darwin build with cgo";
/// a Rust darwin build always can, so the cgo half is gone.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("disclaimed spawn needs a darwin build")]
pub struct ErrUnsupported;

const MH_MAGIC: u32 = 0xfeed_face;
const MH_MAGIC_64: u32 = 0xfeed_facf;
const LC_SEGMENT: u32 = 0x1;
const LC_SEGMENT_64: u32 = 0x19;

/// Whether the Mach-O executable at `path` embeds an Info.plist (a
/// __TEXT,__info_plist section), which is what lets a standalone binary hold
/// its own Local Network permission. It is false for anything that isn't a
/// (thin) Mach-O file, as Go's debug/macho reads it.
pub fn has_info_plist(path: &str) -> bool {
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    macho_sections(&data).is_some_and(|secs| {
        secs.iter()
            .any(|(seg, name)| seg == "__TEXT" && name == "__info_plist")
    })
}

// macho_sections lists (segment, section) names of a thin Mach-O file, or
// None if data isn't one.
fn macho_sections(data: &[u8]) -> Option<Vec<(String, String)>> {
    let magic_le = u32::from_le_bytes(data.get(0..4)?.try_into().ok()?);
    let magic_be = u32::from_be_bytes(data.get(0..4)?.try_into().ok()?);
    let (big, wide) = match (magic_le, magic_be) {
        (MH_MAGIC, _) => (false, false),
        (MH_MAGIC_64, _) => (false, true),
        (_, MH_MAGIC) => (true, false),
        (_, MH_MAGIC_64) => (true, true),
        _ => return None,
    };
    let u32_at = |off: usize| -> Option<u32> {
        let b: [u8; 4] = data.get(off..off + 4)?.try_into().ok()?;
        Some(if big {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    };
    let name_at = |off: usize| -> Option<String> {
        let b = data.get(off..off + 16)?;
        let n = b.iter().position(|&c| c == 0).unwrap_or(16);
        Some(String::from_utf8_lossy(&b[..n]).into_owned())
    };
    let ncmds = u32_at(16)? as usize;
    let sizeofcmds = u32_at(20)? as usize;
    let header: usize = if wide { 32 } else { 28 };
    let cmds_end = header.checked_add(sizeofcmds)?;
    if cmds_end > data.len() {
        return None;
    }
    let mut out = Vec::new();
    let mut off = header;
    for _ in 0..ncmds {
        let cmd = u32_at(off)?;
        let cmdsize = u32_at(off + 4)? as usize;
        if cmdsize < 8 || off + cmdsize > cmds_end {
            return None;
        }
        let (seg_size, sect_size, nsects_off) = match cmd {
            LC_SEGMENT => (56, 68, 48),
            LC_SEGMENT_64 => (72, 80, 64),
            _ => {
                off += cmdsize;
                continue;
            }
        };
        let nsects = u32_at(off + nsects_off)? as usize;
        if seg_size + nsects.checked_mul(sect_size)? > cmdsize {
            return None;
        }
        for i in 0..nsects {
            let s = off + seg_size + i * sect_size;
            // section: sectname[16], segname[16], ...
            out.push((name_at(s + 16)?, name_at(s)?));
        }
        off += cmdsize;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // macho64 builds a minimal 64-bit little-endian Mach-O with one
    // __TEXT segment holding the named sections.
    fn macho64(sections: &[&str]) -> Vec<u8> {
        let mut cmd = Vec::new();
        let cmdsize = 72 + 80 * sections.len() as u32;
        cmd.extend(LC_SEGMENT_64.to_le_bytes());
        cmd.extend(cmdsize.to_le_bytes());
        cmd.extend(name16("__TEXT"));
        cmd.extend([0u8; 32]); // vmaddr, vmsize, fileoff, filesize
        cmd.extend([0u8; 8]); // maxprot, initprot
        cmd.extend((sections.len() as u32).to_le_bytes());
        cmd.extend([0u8; 4]); // flags
        for s in sections {
            cmd.extend(name16(s));
            cmd.extend(name16("__TEXT"));
            cmd.extend([0u8; 48]);
        }
        let mut out = Vec::new();
        out.extend(MH_MAGIC_64.to_le_bytes());
        out.extend(0x0100_000c_u32.to_le_bytes()); // arm64
        out.extend(0u32.to_le_bytes());
        out.extend(2u32.to_le_bytes()); // MH_EXECUTE
        out.extend(1u32.to_le_bytes()); // ncmds
        out.extend((cmd.len() as u32).to_le_bytes());
        out.extend(0u32.to_le_bytes()); // flags
        out.extend(0u32.to_le_bytes()); // reserved
        out.extend(cmd);
        out
    }

    fn name16(s: &str) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..s.len()].copy_from_slice(s.as_bytes());
        b
    }

    #[test]
    fn has_info_plist_reads_mach_o_sections() {
        let dir = tempfile::Builder::new().tempdir_in("/var/tmp").unwrap();
        let with = dir.path().join("with");
        let without = dir.path().join("without");
        let not_macho = dir.path().join("elf");
        std::fs::write(&with, macho64(&["__text", "__info_plist"])).unwrap();
        std::fs::write(&without, macho64(&["__text"])).unwrap();
        std::fs::write(&not_macho, b"\x7fELF not a mach-o file at all").unwrap();
        assert!(has_info_plist(with.to_str().unwrap()));
        assert!(!has_info_plist(without.to_str().unwrap()));
        assert!(!has_info_plist(not_macho.to_str().unwrap()));
        assert!(!has_info_plist("/var/tmp/agent-base-no-such-file"));
        // A truncated file isn't one.
        let mut cut = macho64(&["__info_plist"]);
        cut.truncate(cut.len() - 10);
        std::fs::write(&with, cut).unwrap();
        assert!(!has_info_plist(with.to_str().unwrap()));
    }
}
