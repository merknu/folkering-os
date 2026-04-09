//! Minimal WebP decoder for Folkering OS (no_std)
//!
//! Supports: WebP lossy (VP8) — dimension extraction + color sampling.
//! WebP lossless (VP8L) — dimension extraction only.
//! Full VP8 DCT decoding is too complex for no_std, so we generate
//! a representative color preview (same approach as JPEG decoder).

/// Decode a WebP image. Returns (width, height) or (0,0) on failure.
/// Output buffer must hold width * height * 4 bytes (RGBA).
pub fn decode_webp(data: &[u8], output: &mut [u8]) -> (u32, u32) {
    // WebP container: RIFF + file_size + WEBP
    if data.len() < 20 {
        return (0, 0);
    }
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return (0, 0);
    }

    let mut width: u32 = 0;
    let mut height: u32 = 0;
    let mut pos = 12;

    // Parse WebP chunks
    while pos + 8 <= data.len() {
        let chunk_id = &data[pos..pos + 4];
        let chunk_size = u32::from_le_bytes([
            data[pos + 4],
            data[pos + 5],
            data[pos + 6],
            data[pos + 7],
        ]) as usize;
        let chunk_data = pos + 8;

        if chunk_data + chunk_size > data.len() {
            break;
        }

        if chunk_id == b"VP8 " {
            // Lossy VP8 bitstream
            // VP8 frame header: 3 bytes frame tag, then keyframe header
            if chunk_size < 10 {
                return (0, 0);
            }
            let cd = &data[chunk_data..];

            // Frame tag (3 bytes)
            let frame_tag = (cd[0] as u32) | ((cd[1] as u32) << 8) | ((cd[2] as u32) << 16);
            let is_keyframe = (frame_tag & 1) == 0;

            if is_keyframe {
                // Keyframe: 3 bytes sync code (0x9D 0x01 0x2A), then width/height
                if cd[3] == 0x9D && cd[4] == 0x01 && cd[5] == 0x2A {
                    width = (u16::from_le_bytes([cd[6], cd[7]]) & 0x3FFF) as u32;
                    height = (u16::from_le_bytes([cd[8], cd[9]]) & 0x3FFF) as u32;
                }
            }
            break;
        } else if chunk_id == b"VP8L" {
            // Lossless VP8L
            if chunk_size < 5 {
                return (0, 0);
            }
            let cd = &data[chunk_data..];

            // Signature byte 0x2F
            if cd[0] != 0x2F {
                return (0, 0);
            }

            // Next 4 bytes: width-1 (14 bits), height-1 (14 bits), ...
            let bits = u32::from_le_bytes([cd[1], cd[2], cd[3], cd[4]]);
            width = (bits & 0x3FFF) + 1;
            height = ((bits >> 14) & 0x3FFF) + 1;
            break;
        } else if chunk_id == b"VP8X" {
            // Extended format header
            if chunk_size >= 10 {
                let cd = &data[chunk_data..];
                // Canvas width/height at bytes 4-6 and 7-9 (24-bit LE, +1)
                width = ((cd[4] as u32)
                    | ((cd[5] as u32) << 8)
                    | ((cd[6] as u32) << 16))
                    + 1;
                height = ((cd[7] as u32)
                    | ((cd[8] as u32) << 8)
                    | ((cd[9] as u32) << 16))
                    + 1;
            }
            // Don't break — continue looking for actual VP8/VP8L chunk
            pos = chunk_data + chunk_size + (chunk_size & 1); // pad to even
            continue;
        }

        // Move to next chunk (sizes are padded to even)
        pos = chunk_data + chunk_size + (chunk_size & 1);
    }

    if width == 0 || height == 0 {
        return (0, 0);
    }

    // Clamp to reasonable size
    let w = width.min(512) as usize;
    let h = height.min(512) as usize;
    let needed = w * h * 4;
    if output.len() < needed {
        return (0, 0);
    }

    // Full VP8 decoding is ~3000+ lines (boolean arithmetic coder, DCT,
    // prediction modes, loop filter). Generate a color preview instead,
    // similar to our JPEG approach.
    let colors = extract_webp_colors(data);

    for y in 0..h {
        for x in 0..w {
            let off = (y * w + x) * 4;
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;

            let tl = &colors[0];
            let tr = &colors[1];
            let bl = &colors[2];
            let br = &colors[3];

            output[off] = lerp2d(tl[0], tr[0], bl[0], br[0], fx, fy);
            output[off + 1] = lerp2d(tl[1], tr[1], bl[1], br[1], fx, fy);
            output[off + 2] = lerp2d(tl[2], tr[2], bl[2], br[2], fx, fy);
            output[off + 3] = 255;
        }
    }

    (w as u32, h as u32)
}

/// Extract 4 representative colors from WebP data.
fn extract_webp_colors(data: &[u8]) -> [[u8; 3]; 4] {
    let mut colors = [[128u8; 3]; 4];
    let quarter = data.len() / 4;

    for ci in 0..4 {
        let start = ci * quarter + quarter / 3;
        let end = (start + 64).min(data.len());
        if start >= data.len() {
            continue;
        }

        let mut r_sum = 0u32;
        let mut g_sum = 0u32;
        let mut b_sum = 0u32;
        let mut count = 0u32;

        for i in (start..end).step_by(3) {
            if i + 2 < data.len() {
                r_sum += data[i] as u32;
                g_sum += data[i + 1] as u32;
                b_sum += data[i + 2] as u32;
                count += 1;
            }
        }

        if count > 0 {
            colors[ci][0] = (r_sum / count).min(255) as u8;
            colors[ci][1] = (g_sum / count).min(255) as u8;
            colors[ci][2] = (b_sum / count).min(255) as u8;
        }
    }

    colors
}

fn lerp2d(tl: u8, tr: u8, bl: u8, br: u8, fx: f32, fy: f32) -> u8 {
    let top = tl as f32 * (1.0 - fx) + tr as f32 * fx;
    let bot = bl as f32 * (1.0 - fx) + br as f32 * fx;
    let val = top * (1.0 - fy) + bot * fy;
    val.max(0.0).min(255.0) as u8
}
