//! Minimal deterministic PNG writer for 8-bit grayscale frames.
//!
//! Uses stored (uncompressed) deflate blocks, so output bytes are fully
//! deterministic across runs and platforms — required for evidence artifacts.

fn crc32(data: &[u8]) -> u32 {
    // Standard PNG CRC-32 (IEEE 802.3), table-free bitwise form: small and
    // only run over a few hundred bytes per frame export.
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    let mut crc_input = Vec::with_capacity(4 + payload.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(payload);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    out.push(0x78);
    out.push(0x01);
    let mut offset = 0;
    loop {
        let remaining = data.len() - offset;
        let take = remaining.min(65535);
        let last = offset + take >= data.len();
        out.push(if last { 1 } else { 0 });
        out.extend_from_slice(&(take as u16).to_le_bytes());
        out.extend_from_slice(&(!(take as u16)).to_le_bytes());
        out.extend_from_slice(&data[offset..offset + take]);
        offset += take;
        if last {
            break;
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// Encode a grayscale image as PNG. `pixels` is row-major, one byte per pixel
/// (0 = black, 255 = white), `width` x `height` pixels.
pub fn write_gray_png(pixels: &[u8], width: u32, height: u32) -> Vec<u8> {
    assert_eq!(pixels.len(), (width as usize) * (height as usize));
    let mut raw = Vec::with_capacity((width as usize + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0); // filter: none
        raw.extend_from_slice(&pixels[y * width as usize..(y + 1) * width as usize]);
    }

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(0); // color type: grayscale
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_signature_and_size() {
        let px = vec![0u8; 160 * 43];
        let png = write_gray_png(&px, 160, 43);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert!(png.len() > 100);
        // Deterministic: same input, same bytes.
        let png2 = write_gray_png(&px, 160, 43);
        assert_eq!(png, png2);
    }
}
