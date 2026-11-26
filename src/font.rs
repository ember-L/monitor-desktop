//! macOS 系统中文字体加载：从 TTC 字体集合中提取单个 TTF 供 egui 使用。
//! egui（ab_glyph）不支持 TTC 集合，这里把第 index 个字体重组为独立 TTF。

/// 从 TTC（ttcf）中提取第 `index` 个字体，返回独立 TTF 字节。
pub fn extract_ttf(ttc: &[u8], index: usize) -> Option<Vec<u8>> {
    if ttc.len() < 16 || &ttc[0..4] != b"ttcf" {
        return None;
    }
    let num_fonts = u32::from_be_bytes([ttc[8], ttc[9], ttc[10], ttc[11]]) as usize;
    if index >= num_fonts || 12 + 4 * (index + 1) > ttc.len() {
        return None;
    }
    let o = 12 + 4 * index;
    let off = u32::from_be_bytes([ttc[o], ttc[o + 1], ttc[o + 2], ttc[o + 3]]) as usize;
    if off + 12 > ttc.len() {
        return None;
    }
    let num_tables = u16::from_be_bytes([ttc[off + 4], ttc[off + 5]]) as usize;
    if off + 12 + 16 * num_tables > ttc.len() {
        return None;
    }
    // 独立 TTF：12 字节头 + 表目录 + 表数据（4 字节对齐）
    let mut out = Vec::with_capacity(ttc.len() / num_fonts.max(1) + 64);
    out.extend_from_slice(&ttc[off..off + 12]);
    let mut records = Vec::with_capacity(num_tables);
    let mut data = Vec::new();
    let data_start = 12 + 16 * num_tables;
    for i in 0..num_tables {
        let r = off + 12 + 16 * i;
        let tag = &ttc[r..r + 4];
        let checksum = &ttc[r + 4..r + 8];
        let toff = u32::from_be_bytes([ttc[r + 8], ttc[r + 9], ttc[r + 10], ttc[r + 11]]) as usize;
        let tlen = u32::from_be_bytes([ttc[r + 12], ttc[r + 13], ttc[r + 14], ttc[r + 15]]) as usize;
        if toff.checked_add(tlen).map_or(true, |e| e > ttc.len()) {
            return None;
        }
        let new_off = (data_start + data.len()) as u32;
        let mut rec = Vec::with_capacity(16);
        rec.extend_from_slice(tag);
        rec.extend_from_slice(checksum);
        rec.extend_from_slice(&new_off.to_be_bytes());
        rec.extend_from_slice(&(tlen as u32).to_be_bytes());
        records.push(rec);
        data.extend_from_slice(&ttc[toff..toff + tlen]);
        while data.len() % 4 != 0 {
            data.push(0);
        }
    }
    for rec in records {
        out.extend_from_slice(&rec);
    }
    out.extend_from_slice(&data);
    Some(out)
}

/// 依次尝试系统里的中文字体，返回可供 egui 使用的字体字节。
pub fn load_cjk_font() -> Option<Vec<u8>> {
    const CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/Library/Fonts/Arial Unicode.ttf",
    ];
    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else { continue };
        if bytes.starts_with(b"ttcf") {
            if let Some(f) = extract_ttf(&bytes, 0) {
                return Some(f);
            }
        } else if bytes.len() > 12 {
            return Some(bytes);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(tag);
        v.extend_from_slice(&[0, 0, 0, 0]); // checksum
        v.extend_from_slice(&(data.len() as u32).to_be_bytes()); // 占位，后面填 offset
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
        v.extend_from_slice(data);
        v
    }

    /// 构造一个最小 TTC：两个字体，各含一张表。
    fn fake_ttc() -> Vec<u8> {
        let f0 = table(b"head", &[1, 2, 3]);
        let f1 = table(b"glyf", &[9, 8, 7, 6, 5]);
        let mut ttc = b"ttcf".to_vec();
        ttc.extend_from_slice(&1u32.to_be_bytes()); // version
        ttc.extend_from_slice(&2u32.to_be_bytes()); // numFonts
        let off0 = 20u32;
        let off1 = off0 + 12 + f0.len() as u32;
        ttc.extend_from_slice(&off0.to_be_bytes());
        ttc.extend_from_slice(&off1.to_be_bytes());
        for (off, f) in [(off0, &f0), (off1, &f1)] {
            let mut sfnt = Vec::new();
            sfnt.extend_from_slice(&0x00010000u32.to_be_bytes());
            sfnt.extend_from_slice(&1u16.to_be_bytes()); // numTables
            sfnt.extend_from_slice(&[0; 6]);
            let mut rec = f[..16].to_vec();
            let abs = off + 12 + 16;
            rec[8..12].copy_from_slice(&abs.to_be_bytes());
            sfnt.extend_from_slice(&rec);
            sfnt.extend_from_slice(&f[16..]);
            ttc.extend_from_slice(&sfnt);
        }
        ttc
    }

    #[test]
    fn extracts_both_fonts() {
        let ttc = fake_ttc();
        let a = extract_ttf(&ttc, 0).unwrap();
        assert_eq!(&a[0..4], &0x00010000u32.to_be_bytes());
        // 表目录里 offset 指向 28（12 头 + 16 目录）
        assert_eq!(u32::from_be_bytes([a[20], a[21], a[22], a[23]]), 28);
        assert_eq!(&a[28..31], &[1, 2, 3]);
        let b = extract_ttf(&ttc, 1).unwrap();
        assert_eq!(&b[12..16], b"glyf");
        assert_eq!(&b[28..33], &[9, 8, 7, 6, 5]);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(extract_ttf(b"not a font", 0).is_none());
        let ttc = fake_ttc();
        assert!(extract_ttf(&ttc, 2).is_none());
        assert!(extract_ttf(&ttc[..10], 0).is_none());
    }
}
