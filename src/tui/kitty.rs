//! Kitty graphics protocol primitives.
//!
//! <https://sw.kovidgoyal.net/kitty/graphics-protocol/>
//!
//! Only the core of the protocol is used, so any terminal implementing it
//! works: support is detected with a query action (`a=q`), images are sent by
//! direct transmission (`t=d`) as zlib-compressed RGB (`f=24,o=z`), shown with
//! placements sized in cells (`a=p,c=,r=`) at the cursor, and removed by id
//! (`a=d`). Every command sets `q=2` so the terminal never answers into the
//! input stream.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use base64::Engine;

/// Maximum base64 payload per escape sequence, per the specification.
const CHUNK: usize = 4096;

/// Size of one terminal cell in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellPx {
    pub w: f64,
    pub h: f64,
}

impl Default for CellPx {
    fn default() -> Self {
        Self { w: 10.0, h: 20.0 }
    }
}

/// Image data ready for transmission: zlib-compressed RGB, base64-encoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Encoded {
    pub width: u32,
    pub height: u32,
    b64: String,
}

/// Compress and encode RGB pixels (the expensive part; done off-thread).
pub fn encode(width: u32, height: u32, rgb: &[u8]) -> Encoded {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    z.write_all(rgb).expect("writing to a Vec");
    let compressed = z.finish().expect("writing to a Vec");
    Encoded { width, height, b64: base64::engine::general_purpose::STANDARD.encode(compressed) }
}

/// Transmit an image under `id` without displaying it, in chunks of at most
/// 4096 bytes as the specification requires.
pub fn transmit(id: u32, img: &Encoded) -> String {
    let (width, height, b64) = (img.width, img.height, &img.b64);
    let mut out = String::with_capacity(b64.len() + b64.len() / CHUNK * 24 + 64);
    let chunks: Vec<&str> =
        b64.as_bytes().chunks(CHUNK).map(|c| std::str::from_utf8(c).expect("base64 is ASCII")).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            out.push_str(&format!("\x1b_Ga=t,t=d,f=24,o=z,s={width},v={height},i={id},q=2,m={more};"));
        } else {
            // Continuation chunks may only carry `m` (and `q`).
            out.push_str(&format!("\x1b_Gm={more},q=2;"));
        }
        out.push_str(chunk);
        out.push_str("\x1b\\");
    }
    out
}

/// Display a transmitted image scaled to `cols`×`rows` cells with its top-left
/// corner at cell (`x`, `y`). The cursor is saved and restored around it.
pub fn place(id: u32, x: u16, y: u16, cols: u16, rows: u16) -> String {
    format!(
        "\x1b7\x1b[{};{}H\x1b_Ga=p,i={id},p=1,c={cols},r={rows},C=1,q=2\x1b\\\x1b8",
        y as u32 + 1,
        x as u32 + 1
    )
}

/// Remove an image's placement but keep its data for re-use.
pub fn unplace(id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={id},p=1,q=2\x1b\\")
}

/// Remove an image's placements and free its data.
pub fn free(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

/// What the terminal reported during detection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detection {
    pub supported: bool,
    pub cell: Option<CellPx>,
}

const QUERY_ID: u32 = 31;

/// Ask the terminal whether it implements the graphics protocol, and for its
/// cell size. Must run in raw mode before anything else reads the input.
///
/// Sends a graphics query, `CSI 16 t` (cell size) and primary device
/// attributes. Every terminal answers the latter, so its reply marks the end:
/// a graphics reply arriving before it means the protocol is supported.
pub fn detect(timeout: Duration) -> Detection {
    let none = Detection { supported: false, cell: None };
    // SAFETY: isatty on fixed descriptors.
    if unsafe { libc::isatty(0) == 0 || libc::isatty(1) == 0 } {
        return none;
    }
    let mut stdout = std::io::stdout().lock();
    let query = format!("\x1b_Gi={QUERY_ID},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[16t\x1b[c");
    if stdout.write_all(query.as_bytes()).and_then(|_| stdout.flush()).is_err() {
        return none;
    }
    drop(stdout);

    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut stdin = std::io::stdin().lock();
    while !has_da1_reply(&buf) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as libc::c_int) };
        if ready <= 0 {
            break;
        }
        let mut chunk = [0u8; 256];
        match stdin.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    parse_replies(&buf)
}

fn has_da1_reply(buf: &[u8]) -> bool {
    find(buf, b"\x1b[?").is_some_and(|start| buf[start..].contains(&b'c'))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse_replies(buf: &[u8]) -> Detection {
    let text = String::from_utf8_lossy(buf);
    let da1 = text.find("\x1b[?");
    let ok = format!("\x1b_Gi={QUERY_ID};OK");
    let supported = text.find(&ok).is_some_and(|pos| da1.is_none_or(|d| pos < d));
    // `CSI 6 ; height ; width t`
    let cell = text.find("\x1b[6;").and_then(|start| {
        let rest = &text[start + 4..];
        let end = rest.find('t')?;
        let (h, w) = rest[..end].split_once(';')?;
        let (h, w): (f64, f64) = (h.parse().ok()?, w.parse().ok()?);
        (h > 0.0 && w > 0.0).then_some(CellPx { w, h })
    });
    Detection { supported, cell }
}

/// Cell size from the kernel's idea of the window size, if it knows pixels.
pub fn cell_from_winsize() -> Option<CellPx> {
    let ws = ratatui::crossterm::terminal::window_size().ok()?;
    (ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0)
        .then(|| CellPx { w: ws.width as f64 / ws.columns as f64, h: ws.height as f64 / ws.rows as f64 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn small_image_is_one_chunk() {
        let s = transmit(7, &encode(2, 1, &[255, 0, 0, 0, 255, 0]));
        assert!(s.starts_with("\x1b_Ga=t,t=d,f=24,o=z,s=2,v=1,i=7,q=2,m=0;"));
        assert!(s.ends_with("\x1b\\"));
        assert_eq!(s.matches("\x1b_G").count(), 1);

        // The payload is base64 of zlib of the pixels.
        let payload = &s[s.find(';').unwrap() + 1..s.len() - 2];
        let z = base64::engine::general_purpose::STANDARD.decode(payload).unwrap();
        let mut raw = Vec::new();
        flate2::read::ZlibDecoder::new(&z[..]).read_to_end(&mut raw).unwrap();
        assert_eq!(raw, [255, 0, 0, 0, 255, 0]);
    }

    #[test]
    fn large_images_are_chunked_per_spec() {
        // Noise doesn't compress, so this spans several chunks.
        let mut state = 1u32;
        let rgb: Vec<u8> = (0..30_000)
            .map(|_| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (state >> 16) as u8
            })
            .collect();
        let s = transmit(9, &encode(100, 100, &rgb));
        let parts: Vec<&str> = s.split("\x1b\\").filter(|p| !p.is_empty()).collect();
        assert!(parts.len() > 2);
        for (i, part) in parts.iter().enumerate() {
            let (keys, data) = part.trim_start_matches("\x1b_G").split_once(';').unwrap();
            assert!(data.len() <= CHUNK);
            let last = i + 1 == parts.len();
            if i == 0 {
                assert!(keys.contains("a=t") && keys.contains("i=9") && keys.ends_with("m=1"));
            } else {
                assert_eq!(keys, if last { "m=0,q=2" } else { "m=1,q=2" });
            }
            if !last {
                assert_eq!(data.len() % 4, 0, "non-final chunks are whole base64 quads");
            }
        }
    }

    #[test]
    fn placement_and_deletion() {
        assert_eq!(place(3, 0, 4, 20, 10), "\x1b7\x1b[5;1H\x1b_Ga=p,i=3,p=1,c=20,r=10,C=1,q=2\x1b\\\x1b8");
        assert_eq!(unplace(3), "\x1b_Ga=d,d=i,i=3,p=1,q=2\x1b\\");
        assert_eq!(free(3), "\x1b_Ga=d,d=I,i=3,q=2\x1b\\");
    }

    #[test]
    fn parses_detection_replies() {
        let kitty = b"\x1b_Gi=31;OK\x1b\\\x1b[6;20;10t\x1b[?62;22;52c";
        assert!(has_da1_reply(kitty));
        assert_eq!(
            parse_replies(kitty),
            Detection { supported: true, cell: Some(CellPx { w: 10.0, h: 20.0 }) }
        );

        // Only device attributes: no graphics.
        let plain = b"\x1b[?1;2c";
        assert_eq!(parse_replies(plain), Detection { supported: false, cell: None });

        // An error reply to the query isn't support either.
        let err = b"\x1b_Gi=31;EINVAL:bad\x1b\\\x1b[?62c";
        assert!(!parse_replies(err).supported);

        assert!(!has_da1_reply(b"\x1b_Gi=31;OK\x1b\\"));
    }
}
