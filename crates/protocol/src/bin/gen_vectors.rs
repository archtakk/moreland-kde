//! Generates the cross-language protocol conformance corpus.
//!
//! Run with `cargo run -p protocol --bin gen_vectors`. The output is
//! `testdata/protocol/corpus.tsv` at the workspace root, one row per case:
//!
//! ```text
//! name  kind  direction  expect  fields  hex
//! ```
//!
//! `kind` is one of `stream_header`, `frame_header`, `control`, `ack`,
//! `touch`, `reverse`. `direction` is `h2d` (parsed by Kotlin) or `d2h`
//! (encoded by Kotlin, decoded by Rust). `expect` is `ok` or `err`;
//! `fields` is `k=v;k=v;...` on `ok` and a substring of the error message
//! on `err`, empty for "any error".
//!
//! The corpus is read by both:
//! - `crates/protocol/tests/conformance.rs`
//! - `android/app/src/test/java/com/moreland/display/ProtocolConformanceTest.kt`
//!
//! If the two implementations ever disagree about a byte, one of the two
//! tests fails. Regenerate and commit whenever the wire format changes.

use protocol::*;
use std::fs;
use std::path::PathBuf;

struct Case {
    name: String,
    kind: &'static str,
    direction: &'static str,
    expect: &'static str,
    fields: String,
    bytes: Vec<u8>,
}

macro_rules! case {
    ($v:expr, $name:expr, $kind:expr, $dir:expr, ok $fields:expr, $bytes:expr) => {
        $v.push(Case {
            name: $name.into(),
            kind: $kind,
            direction: $dir,
            expect: "ok",
            fields: $fields.into(),
            bytes: $bytes,
        });
    };
    ($v:expr, $name:expr, $kind:expr, $dir:expr, err $substr:expr, $bytes:expr) => {
        $v.push(Case {
            name: $name.into(),
            kind: $kind,
            direction: $dir,
            expect: "err",
            fields: $substr.into(),
            bytes: $bytes,
        });
    };
}

fn cases() -> Vec<Case> {
    let mut v = Vec::new();

    // ---------- stream_header ----------
    let sh = StreamHeader { width: 1920, height: 1200, framerate: 60, codec: Codec::H264 }
        .encode().to_vec();
    case!(v, "stream_header_valid_h264", "stream_header", "h2d",
        ok "width=1920;height=1200;framerate=60;codec=H264", sh.clone());

    let mut sh265 = sh.clone(); sh265[12] = 1;
    case!(v, "stream_header_valid_h265", "stream_header", "h2d",
        ok "width=1920;height=1200;framerate=60;codec=H265", sh265);

    let mut bad = sh.clone(); bad[0..4].copy_from_slice(b"XXXX");
    case!(v, "stream_header_bad_magic", "stream_header", "h2d", err "bad magic", bad);

    for ver in [0u16, 1, 2, 4] {
        let mut buf = sh.clone(); buf[4..6].copy_from_slice(&ver.to_be_bytes());
        case!(v, format!("stream_header_version_{ver}"), "stream_header", "h2d",
            err format!("version {ver}"), buf);
    }

    let mut badc = sh.clone(); badc[12] = 0xff;
    case!(v, "stream_header_unknown_codec", "stream_header", "h2d",
        err "codec id 255", badc);

    case!(v, "stream_header_truncated_8", "stream_header", "h2d",
        err "", sh[..8].to_vec());
    case!(v, "stream_header_truncated_15", "stream_header", "h2d",
        err "", sh[..15].to_vec());

    // ---------- frame_header ----------
    let fh = |len: u32, pts: u64, kf: bool, ctl: bool| -> Vec<u8> {
        FrameHeader { length: len, pts_ns: pts, keyframe: kf, control: ctl }
            .encode().to_vec()
    };
    case!(v, "frame_header_valid_keyframe", "frame_header", "h2d",
        ok "length=32389;pts_ns=1666666560;keyframe=true;control=false",
        fh(32389, 1666666560, true, false));
    case!(v, "frame_header_valid_delta", "frame_header", "h2d",
        ok "length=32389;pts_ns=1666666560;keyframe=false;control=false",
        fh(32389, 1666666560, false, false));
    case!(v, "frame_header_control", "frame_header", "h2d",
        ok "length=2;pts_ns=0;keyframe=false;control=true",
        fh(2, 0, false, true));
    case!(v, "frame_header_both_flags", "frame_header", "h2d",
        ok "length=2;pts_ns=0;keyframe=true;control=true",
        fh(2, 0, true, true));
    case!(v, "frame_header_zero_length", "frame_header", "h2d",
        ok "length=0;pts_ns=0;keyframe=false;control=false",
        fh(0, 0, false, false));
    let mut unknown = fh(1, 0, false, false); unknown[12] = 0x04;
    case!(v, "frame_header_unknown_flag_bit", "frame_header", "h2d",
        ok "length=1;pts_ns=0;keyframe=false;control=false", unknown);
    case!(v, "frame_header_truncated_8", "frame_header", "h2d",
        err "", fh(1, 0, false, false)[..8].to_vec());

    // ---------- control ----------
    let ctl = |k: u8, val: u8| vec![k, val];
    case!(v, "control_brightness_0", "control", "h2d",
        ok "kind=Brightness;value=0", ctl(0x01, 0));
    case!(v, "control_brightness_100", "control", "h2d",
        ok "kind=Brightness;value=100", ctl(0x01, 100));
    case!(v, "control_rotation_0", "control", "h2d",
        ok "kind=Rotation;value=0", ctl(0x02, 0));
    case!(v, "control_rotation_270", "control", "h2d",
        ok "kind=Rotation;value=3", ctl(0x02, 3));
    case!(v, "control_rotation_out_of_range", "control", "h2d",
        ok "kind=Rotation;value=255", ctl(0x02, 0xff));
    case!(v, "control_unknown_kind", "control", "h2d",
        err "unknown control kind", ctl(0xff, 0));
    case!(v, "control_unknown_kind_zero", "control", "h2d",
        err "unknown control kind", ctl(0x00, 0));
    case!(v, "control_truncated_1", "control", "h2d", err "", vec![0x01]);
    case!(v, "control_truncated_0", "control", "h2d", err "", vec![]);

    // ---------- ack (device -> host) ----------
    case!(v, "ack_zero", "ack", "d2h", ok "pts_ns=0", encode_ack(0).to_vec());
    case!(v, "ack_typical", "ack", "d2h",
        ok "pts_ns=1666666560", encode_ack(1666666560).to_vec());
    case!(v, "ack_max", "ack", "d2h",
        ok "pts_ns=18446744073709551615", encode_ack(u64::MAX).to_vec());

    // ---------- touch (device -> host) ----------
    let tm = |a: TouchAction, x: u16, y: u16| -> Vec<u8> {
        encode_touch(&TouchMessage { action: a, x, y }).to_vec()
    };
    case!(v, "touch_down_0_0", "touch", "d2h",
        ok "action=Down;x=0;y=0", tm(TouchAction::Down, 0, 0));
    case!(v, "touch_move_center", "touch", "d2h",
        ok "action=Move;x=32768;y=32768", tm(TouchAction::Move, 32768, 32768));
    case!(v, "touch_up_max", "touch", "d2h",
        ok "action=Up;x=65535;y=65535", tm(TouchAction::Up, 65535, 65535));
    case!(v, "touch_cancel", "touch", "d2h",
        ok "action=Cancel;x=4660;y=43981", tm(TouchAction::Cancel, 0x1234, 0xabcd));

    // ---------- reverse, decoder-only cases (Rust only) ----------
    case!(v, "reverse_unknown_type", "reverse", "d2h",
        err "unknown reverse message type", vec![0xee, 0, 0, 0, 0, 0, 0, 0, 0]);
    case!(v, "reverse_truncated", "reverse", "d2h",
        err "", vec![0x01, 0x00, 0x00, 0x00]);
    case!(v, "touch_unknown_action", "reverse", "d2h",
        err "unknown touch action", vec![0x02, 0x7f, 0, 0, 0, 0, 0, 0, 0]);

    v
}

fn main() -> std::io::Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/protocol");
    fs::create_dir_all(&root)?;
    let path = root.join("corpus.tsv");

    let cases = cases();
    let mut out = String::new();
    out.push_str("# moreland protocol conformance corpus\n");
    out.push_str(&format!("# protocol VERSION: {}\n", protocol::VERSION));
    out.push_str("# columns: name\\tkind\\tdirection\\texpect\\tfields\\thex\n");
    for c in &cases {
        let hex: String = c.bytes.iter().map(|b| format!("{b:02x}")).collect();
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            c.name, c.kind, c.direction, c.expect, c.fields, hex,
        ));
    }
    fs::write(&path, out)?;
    println!("wrote {} cases to {}", cases.len(), path.display());
    Ok(())
}
