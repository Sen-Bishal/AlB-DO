//! One-off helper to decode an inline opcode frame from base64 and
//! print its instructions. Used to diagnose the post-P inline
//! bootstrap. Delete once the bug is closed.

use dom_render_compiler::ir::wire::decode_frame;

fn decode_b64(s: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let core: String = s.chars().filter(|c| *c != '=' && !c.is_whitespace()).collect();
    let chars: Vec<u8> = core.bytes().map(|b| ALPHABET.iter().position(|&x| x == b).unwrap_or(0) as u8).collect();
    let mut out = Vec::new();
    for chunk in chars.chunks(4) {
        let mut n: u32 = 0;
        for &c in chunk { n = (n << 6) | (c as u32); }
        let pad_in_chunk = if chunk.len() < 4 { 4 - chunk.len() } else { 0 };
        n <<= 6 * pad_in_chunk;
        let bytes_in = 3 - pad_in_chunk;
        if bytes_in >= 1 { out.push((n >> 16) as u8); }
        if bytes_in >= 2 { out.push((n >> 8) as u8); }
        if bytes_in >= 3 { out.push(n as u8); }
    }
    out
}

#[test]
fn decode_dev_smoke_frame() {
    let frames = [
        ("home", "AAAEDfx2IUINBG51bGwAAgEBBWNsaWNrB/yUK8JjAfysKSIlC/xNMMJm/EZQpY0="),
        ("chat", "AAACDfx2IUINBG51bGwL/AQDqR38diFCDQ=="),
    ];
    for (label, b64) in frames {
        let bytes = decode_b64(b64);
        eprintln!("\n=== {} frame ({} bytes) ===", label, bytes.len());
        eprintln!("First bytes (hex): {:02x?}", &bytes[..bytes.len().min(15)]);
        match decode_frame(&bytes) {
            Ok((frame, consumed)) => {
                eprintln!("frame_id = {}", frame.frame_id);
                eprintln!("component_id = {:?}", frame.component_id);
                eprintln!("instructions ({} total):", frame.instructions.len());
                for (i, inst) in frame.instructions.iter().enumerate() {
                    eprintln!("  [{i}] {inst:?}");
                }
                eprintln!("consumed = {} of {}", consumed, bytes.len());
            }
            Err(err) => eprintln!("decode FAILED: {err:?}"),
        }
    }
}
