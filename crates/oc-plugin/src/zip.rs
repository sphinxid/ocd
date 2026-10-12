//! Minimal ZIP reader: extracts one entry (stored or deflated) from an in-memory archive.
//! Enough for Bun's release archives without pulling in a zip crate.

use std::io::Read;

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    bytes.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Returns the bytes of the entry named `name`, `Ok(None)` when absent.
pub fn extract(archive: &[u8], name: &str) -> Result<Option<Vec<u8>>, String> {
    // End of central directory: signature 0x06054b50 within the last 64 KiB + 22 bytes.
    let start = archive.len().saturating_sub(22 + 65_535);
    let eocd = (start..archive.len().saturating_sub(21))
        .rev()
        .find(|&i| u32_at(archive, i) == Some(0x0605_4b50))
        .ok_or("invalid zip archive: end of central directory not found")?;
    let entries = u16_at(archive, eocd + 10).ok_or("invalid zip archive")? as usize;
    let mut cursor = u32_at(archive, eocd + 16).ok_or("invalid zip archive")? as usize;
    for _ in 0..entries {
        if u32_at(archive, cursor) != Some(0x0201_4b50) {
            return Err("invalid zip archive: bad central directory entry".into());
        }
        let method = u16_at(archive, cursor + 10).ok_or("invalid zip archive")?;
        let compressed = u32_at(archive, cursor + 20).ok_or("invalid zip archive")? as usize;
        let size = u32_at(archive, cursor + 24).ok_or("invalid zip archive")? as usize;
        let name_len = u16_at(archive, cursor + 28).ok_or("invalid zip archive")? as usize;
        let extra_len = u16_at(archive, cursor + 30).ok_or("invalid zip archive")? as usize;
        let comment_len = u16_at(archive, cursor + 32).ok_or("invalid zip archive")? as usize;
        let local = u32_at(archive, cursor + 42).ok_or("invalid zip archive")? as usize;
        let entry_name = archive.get(cursor + 46..cursor + 46 + name_len).ok_or("invalid zip archive")?;
        cursor += 46 + name_len + extra_len + comment_len;
        if entry_name != name.as_bytes() {
            continue;
        }
        if u32_at(archive, local) != Some(0x0403_4b50) {
            return Err("invalid zip archive: bad local header".into());
        }
        let local_name = u16_at(archive, local + 26).ok_or("invalid zip archive")? as usize;
        let local_extra = u16_at(archive, local + 28).ok_or("invalid zip archive")? as usize;
        let data_start = local + 30 + local_name + local_extra;
        let data = archive.get(data_start..data_start + compressed).ok_or("invalid zip archive: truncated entry")?;
        let bytes = match method {
            0 => data.to_vec(),
            8 => {
                let mut out = Vec::with_capacity(size);
                flate2::read::DeflateDecoder::new(data).read_to_end(&mut out).map_err(|e| e.to_string())?;
                out
            }
            other => return Err(format!("unsupported zip compression method {other}")),
        };
        if bytes.len() != size {
            return Err("invalid zip archive: size mismatch".into());
        }
        return Ok(Some(bytes));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a one-entry archive (stored or deflated).
    fn archive(name: &str, content: &[u8], deflate: bool) -> Vec<u8> {
        use std::io::Write;
        let data = if deflate {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(content).unwrap();
            e.finish().unwrap()
        } else {
            content.to_vec()
        };
        let method: u16 = if deflate { 8 } else { 0 };
        let mut out = Vec::new();
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend([20, 0, 0, 0]);
        out.extend(method.to_le_bytes());
        out.extend([0; 8]); // time, date, crc
        out.extend((data.len() as u32).to_le_bytes());
        out.extend((content.len() as u32).to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(name.as_bytes());
        out.extend(&data);
        let central = out.len();
        out.extend(0x0201_4b50u32.to_le_bytes());
        out.extend([20, 0, 20, 0, 0, 0]);
        out.extend(method.to_le_bytes());
        out.extend([0; 8]);
        out.extend((data.len() as u32).to_le_bytes());
        out.extend((content.len() as u32).to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend([0; 12]); // extra, comment, disk, internal attrs, external attrs (4)
        out.extend(0u32.to_le_bytes()); // local header offset
        out.extend(name.as_bytes());
        let size = out.len() - central;
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0; 4]);
        out.extend(1u16.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend((size as u32).to_le_bytes());
        out.extend((central as u32).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    #[test]
    fn extracts_entries() {
        for deflate in [false, true] {
            let zip = archive("bun-linux-x64/bun", b"hello hello hello", deflate);
            assert_eq!(extract(&zip, "bun-linux-x64/bun").unwrap().unwrap(), b"hello hello hello");
            assert_eq!(extract(&zip, "other").unwrap(), None);
        }
        assert!(extract(b"nope", "x").is_err());
    }
}
