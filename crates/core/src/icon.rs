//! The app icon, drawn in code (no image dependency): a rounded tile with a flame. The flame colour and a
//! corner badge show the scheduler state, so the tray (and the GUI window) can tell it at a glance.
//! UI-free: callers get RGBA bytes (GUI), ARGB32 bytes (tray) or a PNG (packaging).
use crate::control::RunState;

/// What the icon says about the scheduler. See [`IconState::derive`] for the priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    /// A task is executing.
    Running,
    /// Automation is on and idle, waiting for capacity / the schedule.
    Waiting,
    /// Automation is switched off.
    Disabled,
    /// Paused by the user.
    Paused,
    /// Emergency STOP is engaged.
    Stopped,
    /// This process is read-only and no driver answers on the control socket.
    NoDriver,
}

impl IconState {
    pub const ALL: [IconState; 6] = [
        IconState::Running,
        IconState::Waiting,
        IconState::Disabled,
        IconState::Paused,
        IconState::Stopped,
        IconState::NoDriver,
    ];

    /// Priority: Stopped > Paused > NoDriver > Running > Disabled > Waiting.
    pub fn derive(enabled: bool, run: RunState, driver_unreachable: bool) -> IconState {
        match run {
            RunState::Stopped => IconState::Stopped,
            RunState::Paused => IconState::Paused,
            _ if driver_unreachable => IconState::NoDriver,
            RunState::Running => IconState::Running,
            RunState::Waiting if !enabled => IconState::Disabled,
            RunState::Waiting => IconState::Waiting,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            IconState::Running => "running",
            IconState::Waiting => "waiting",
            IconState::Disabled => "disabled",
            IconState::Paused => "paused",
            IconState::Stopped => "stopped",
            IconState::NoDriver => "nodriver",
        }
    }

    pub fn parse(s: &str) -> Option<IconState> {
        IconState::ALL.into_iter().find(|st| st.label() == s)
    }
}

/// Full description of one icon: the state plus the "approvals waiting" dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IconSpec {
    pub state: IconState,
    pub attention: bool,
}

type Rgba = [f32; 4];

const fn rgb(hex: u32) -> Rgba {
    [
        ((hex >> 16) & 0xff) as f32 / 255.0,
        ((hex >> 8) & 0xff) as f32 / 255.0,
        (hex & 0xff) as f32 / 255.0,
        1.0,
    ]
}

const WHITE: Rgba = rgb(0xffffff);
const BADGE_RING: Rgba = rgb(0x14141a);
const BLUE: Rgba = rgb(0x3b82f6);

struct Palette {
    tile: Rgba,
    flame: Rgba,
    core: Rgba,
}

fn palette(s: IconState) -> Palette {
    match s {
        IconState::Running => Palette {
            tile: rgb(0x26262e),
            flame: rgb(0xff7a1a),
            core: rgb(0xffd34d),
        },
        IconState::Waiting => Palette {
            tile: rgb(0x26262e),
            flame: rgb(0x3ecf8e),
            core: rgb(0xb8f5d8),
        },
        IconState::Paused => Palette {
            tile: rgb(0x26262e),
            flame: rgb(0xf5a524),
            core: rgb(0xffe08a),
        },
        IconState::Stopped => Palette {
            tile: rgb(0x5a1a1f),
            flame: rgb(0xff6b6b),
            core: rgb(0xffd0d0),
        },
        IconState::Disabled | IconState::NoDriver => Palette {
            tile: rgb(0x34343a),
            flame: rgb(0x7a7a85),
            core: rgb(0xa8a8b3),
        },
    }
}

fn over(dst: Rgba, src: Rgba) -> Rgba {
    let a = src[3] + dst[3] * (1.0 - src[3]);
    if a <= 0.0 {
        return [0.0; 4];
    }
    let mix = |s: f32, d: f32| (s * src[3] + d * dst[3] * (1.0 - src[3])) / a;
    [
        mix(src[0], dst[0]),
        mix(src[1], dst[1]),
        mix(src[2], dst[2]),
        a,
    ]
}

fn in_round_rect(x: f32, y: f32, c: (f32, f32), half: f32, r: f32) -> bool {
    let qx = (x - c.0).abs() - (half - r);
    let qy = (y - c.1).abs() - (half - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) <= r
}

fn in_circle(x: f32, y: f32, c: (f32, f32), r: f32) -> bool {
    (x - c.0).powi(2) + (y - c.1).powi(2) <= r * r
}

/// Flame outline in tile coordinates (0..1): a round belly with a tip that curls to the right.
/// `scale` shrinks it around the belly for the bright core.
fn in_flame(x: f32, y: f32, scale: f32) -> bool {
    let (cx, cy) = (0.5f32, 0.64f32);
    let (x, y) = (cx + (x - cx) / scale, cy + (y - cy) / scale);
    let (top, belly) = (0.14f32, 0.60f32);
    if y < top || y > cy + 0.2 {
        return false;
    }
    if y >= belly {
        return in_circle(x, y, (cx, cy), 0.2);
    }
    let t = (y - top) / (belly - top);
    let half = 0.2 * t.powf(1.5);
    let lean = 0.07 * (1.0 - t).powi(2);
    (x - cx - lean).abs() <= half
}

fn in_segment(x: f32, y: f32, a: (f32, f32), b: (f32, f32), w: f32) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((x - a.0) * dx + (y - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    in_circle(x, y, (a.0 + t * dx, a.1 + t * dy), w / 2.0)
}

fn sample(spec: IconSpec, x: f32, y: f32) -> Rgba {
    let p = palette(spec.state);
    let mut px = [0.0; 4];
    if !in_round_rect(x, y, (0.5, 0.5), 0.48, 0.22) {
        return px;
    }
    px = p.tile;
    if in_flame(x, y, 1.0) {
        px = over(px, p.flame);
    }
    if in_flame(x, y, 0.5) {
        px = over(px, p.core);
    }
    let badge_at = (0.74, 0.74);
    let badge_bg = match spec.state {
        IconState::Paused => Some(rgb(0x3a3a44)),
        IconState::Stopped => Some(rgb(0xe5484d)),
        IconState::NoDriver => Some(rgb(0x6b6b75)),
        _ => None,
    };
    if let Some(bg) = badge_bg {
        if in_circle(x, y, badge_at, 0.22) {
            px = over(px, BADGE_RING);
        }
        if in_circle(x, y, badge_at, 0.185) {
            px = over(px, bg);
        }
        let glyph = match spec.state {
            IconState::Paused => {
                (0.675..=0.72).contains(&x) && (0.66..=0.82).contains(&y)
                    || (0.76..=0.805).contains(&x) && (0.66..=0.82).contains(&y)
            }
            IconState::Stopped => (0.67..=0.81).contains(&x) && (0.67..=0.81).contains(&y),
            _ => in_segment(x, y, (0.665, 0.665), (0.815, 0.815), 0.045),
        };
        if glyph {
            px = over(px, WHITE);
        }
    }
    if spec.attention {
        if in_circle(x, y, (0.8, 0.2), 0.165) {
            px = over(px, WHITE);
        }
        if in_circle(x, y, (0.8, 0.2), 0.125) {
            px = over(px, BLUE);
        }
    }
    px
}

/// Render `size`x`size` RGBA8 (straight alpha), 4x4 supersampled.
pub fn render_rgba(size: u32, spec: IconSpec) -> Vec<u8> {
    const SS: u32 = 4;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px * SS + sx) as f32 + 0.5;
                    let y = (py * SS + sy) as f32 + 0.5;
                    let c = sample(spec, x / (size * SS) as f32, y / (size * SS) as f32);
                    acc[0] += c[0] * c[3];
                    acc[1] += c[1] * c[3];
                    acc[2] += c[2] * c[3];
                    acc[3] += c[3];
                }
            }
            let n = (SS * SS) as f32;
            let a = acc[3] / n;
            for ch in &acc[..3] {
                let v = if acc[3] > 0.0 { ch / acc[3] } else { 0.0 };
                out.push((v * 255.0).round() as u8);
            }
            out.push((a * 255.0).round() as u8);
        }
    }
    out
}

/// RGBA -> ARGB32 in network byte order (what StatusNotifierItem pixmaps use).
pub fn to_argb_be(rgba: &[u8]) -> Vec<u8> {
    let (px, _) = rgba.as_chunks::<4>();
    px.iter().flat_map(|p| [p[3], p[0], p[1], p[2]]).collect()
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &d in data {
        a = (a + d as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend(data);
    out.extend(&body);
    out.extend(crc32(&body).to_be_bytes());
}

/// Minimal PNG writer (RGBA8, uncompressed "stored" deflate blocks).
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    assert_eq!(rgba.len(), (width * height * 4) as usize);
    let mut raw = Vec::with_capacity(rgba.len() + height as usize);
    for row in rgba.chunks_exact((width * 4) as usize) {
        raw.push(0); // filter: none
        raw.extend(row);
    }
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    while let Some(b) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none()));
        z.extend((b.len() as u16).to_le_bytes());
        z.extend((!(b.len() as u16)).to_le_bytes());
        z.extend(b);
    }
    z.extend(adler32(&raw).to_be_bytes());
    let mut ihdr = Vec::new();
    ihdr.extend(width.to_be_bytes());
    ihdr.extend(height.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]);
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(state: IconState, attention: bool) -> IconSpec {
        IconSpec { state, attention }
    }

    fn px(buf: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn size_corners_and_centre() {
        for s in IconState::ALL {
            let b = render_rgba(32, spec(s, false));
            assert_eq!(b.len(), 32 * 32 * 4);
            assert_eq!(px(&b, 32, 0, 0)[3], 0, "{s:?} corner must be transparent");
            assert_eq!(px(&b, 32, 16, 20)[3], 255, "{s:?} body must be opaque");
        }
    }

    #[test]
    fn every_state_and_the_attention_dot_look_different() {
        let all: Vec<Vec<u8>> = IconState::ALL
            .iter()
            .map(|s| render_rgba(24, spec(*s, false)))
            .collect();
        for i in 0..all.len() {
            for j in i + 1..all.len() {
                assert_ne!(
                    all[i],
                    all[j],
                    "{:?} vs {:?}",
                    IconState::ALL[i],
                    IconState::ALL[j]
                );
            }
        }
        for s in IconState::ALL {
            assert_ne!(
                render_rgba(24, spec(s, false)),
                render_rgba(24, spec(s, true))
            );
        }
        // The dot is blue, top-right.
        let b = render_rgba(64, spec(IconState::Running, true));
        let [r, g, bl, a] = px(&b, 64, 51, 13);
        assert!(a == 255 && bl > r && bl > g);
    }

    #[test]
    fn derive_priority() {
        use RunState::*;
        let d = IconState::derive;
        assert_eq!(d(true, Stopped, true), IconState::Stopped);
        assert_eq!(d(false, Stopped, false), IconState::Stopped);
        assert_eq!(d(false, Paused, true), IconState::Paused);
        assert_eq!(d(true, Running, true), IconState::NoDriver);
        assert_eq!(d(false, Running, false), IconState::Running);
        assert_eq!(d(false, Waiting, false), IconState::Disabled);
        assert_eq!(d(true, Waiting, false), IconState::Waiting);
    }

    #[test]
    fn labels_round_trip() {
        for s in IconState::ALL {
            assert_eq!(IconState::parse(s.label()), Some(s));
        }
        assert_eq!(IconState::parse("nope"), None);
    }

    #[test]
    fn argb_is_network_order() {
        assert_eq!(
            to_argb_be(&[1, 2, 3, 4, 5, 6, 7, 8]),
            [4, 1, 2, 3, 8, 5, 6, 7]
        );
    }

    #[test]
    fn png_is_structurally_valid_and_lossless() {
        let (w, h) = (70, 300); // > 65535 bytes of data: exercises several stored blocks
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i * 31 % 251) as u8).collect();
        let png = encode_png(w, h, &rgba);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        let (mut i, mut idat, mut kinds) = (8, Vec::new(), Vec::new());
        while i < png.len() {
            let len = u32::from_be_bytes(png[i..i + 4].try_into().unwrap()) as usize;
            let body = &png[i + 4..i + 8 + len];
            let crc = u32::from_be_bytes(png[i + 8 + len..i + 12 + len].try_into().unwrap());
            assert_eq!(crc32(body), crc);
            kinds.push(String::from_utf8_lossy(&body[..4]).into_owned());
            if &body[..4] == b"IDAT" {
                idat.extend(&body[4..]);
            }
            if &body[..4] == b"IHDR" {
                assert_eq!(u32::from_be_bytes(body[4..8].try_into().unwrap()), w);
                assert_eq!(u32::from_be_bytes(body[8..12].try_into().unwrap()), h);
                assert_eq!(&body[12..17], &[8, 6, 0, 0, 0]);
            }
            i += 12 + len;
        }
        assert_eq!(kinds, ["IHDR", "IDAT", "IEND"]);
        // Inflate the stored blocks and compare with the input.
        let (mut j, mut raw) = (2, Vec::new());
        loop {
            let last = idat[j] == 1;
            let n = u16::from_le_bytes([idat[j + 1], idat[j + 2]]) as usize;
            assert_eq!(u16::from_le_bytes([idat[j + 3], idat[j + 4]]), !(n as u16));
            raw.extend(&idat[j + 5..j + 5 + n]);
            j += 5 + n;
            if last {
                break;
            }
        }
        assert_eq!(
            u32::from_be_bytes(idat[j..j + 4].try_into().unwrap()),
            adler32(&raw)
        );
        let unfiltered: Vec<u8> = raw
            .chunks_exact(w as usize * 4 + 1)
            .flat_map(|r| {
                assert_eq!(r[0], 0);
                r[1..].to_vec()
            })
            .collect();
        assert_eq!(unfiltered, rgba);
    }
}
