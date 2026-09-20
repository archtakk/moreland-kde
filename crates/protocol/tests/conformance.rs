//! Cross-language protocol conformance test.
//!
//! Reads `testdata/protocol/corpus.tsv` and validates the Rust
//! implementation against it. The Kotlin implementation is validated
//! against the same file by
//! `android/app/src/test/java/com/moreland/display/ProtocolConformanceTest.kt`.
//!
//! Regenerate the corpus with `cargo run -p protocol --bin gen_vectors`
//! whenever the wire format changes. A version bump that misses one side is
//! exactly what this test exists to catch.

use protocol::*;
use std::collections::BTreeMap;
use std::path::PathBuf;

struct Case {
    name: String,
    kind: String,
    direction: String,
    expect: String,
    fields: BTreeMap<String, String>,
    err_substr: String,
    bytes: Vec<u8>,
}

fn load_corpus() -> Vec<Case> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/protocol/corpus.tsv");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));

    let mut out = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') { continue; }
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 6, "line {} has {} columns", lineno + 1, cols.len());
        let hex = cols[5];
        assert_eq!(hex.len() % 2, 0, "odd hex on line {}", lineno + 1);
        let bytes: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i*2..i*2+2], 16).unwrap())
            .collect();

        let expect = cols[3].to_string();
        let mut fields = BTreeMap::new();
        let mut err_substr = String::new();
        if expect == "ok" {
            for pair in cols[4].split(';') {
                if pair.is_empty() { continue; }
                let (k, v) = pair.split_once('=').unwrap();
                fields.insert(k.to_string(), v.to_string());
            }
        } else {
            err_substr = cols[4].to_string();
        }

        out.push(Case {
            name: cols[0].to_string(),
            kind: cols[1].to_string(),
            direction: cols[2].to_string(),
            expect,
            fields,
            err_substr,
            bytes,
        });
    }
    out
}

fn parse(kind: &str, bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    match kind {
        "stream_header" => {
            let h = StreamHeader::decode(bytes).map_err(|e| e.to_string())?;
            out.insert("width".into(), h.width.to_string());
            out.insert("height".into(), h.height.to_string());
            out.insert("framerate".into(), h.framerate.to_string());
            out.insert("codec".into(), format!("{:?}", h.codec));
        }
        "frame_header" => {
            let h = FrameHeader::decode(bytes).map_err(|e| e.to_string())?;
            out.insert("length".into(), h.length.to_string());
            out.insert("pts_ns".into(), h.pts_ns.to_string());
            out.insert("keyframe".into(), h.keyframe.to_string());
            out.insert("control".into(), h.control.to_string());
        }
        "control" => {
            let c = ControlMessage::decode(bytes).map_err(|e| e.to_string())?;
            out.insert("kind".into(), format!("{:?}", c.kind));
            out.insert("value".into(), c.value.to_string());
        }
        "ack" | "touch" | "reverse" => {
            let m = decode_reverse_message(bytes).map_err(|e| e.to_string())?;
            match m {
                ReverseMessage::Ack(pts) => { out.insert("pts_ns".into(), pts.to_string()); }
                ReverseMessage::Touch(t) => {
                    out.insert("action".into(), format!("{:?}", t.action));
                    out.insert("x".into(), t.x.to_string());
                    out.insert("y".into(), t.y.to_string());
                }
            }
        }
        other => return Err(format!("unknown kind {other:?}")),
    }
    Ok(out)
}

#[test]
fn rust_parses_corpus_as_expected() {
    let cases = load_corpus();
    assert!(!cases.is_empty(), "corpus is empty");
    let mut failures = Vec::new();
    for c in &cases {
        let got = parse(&c.kind, &c.bytes);
        match (c.expect.as_str(), got) {
            ("ok", Ok(fields)) => {
                for (k, want) in &c.fields {
                    match fields.get(k) {
                        Some(g) if g == want => {}
                        Some(g) => failures.push(
                            format!("{}: field {k}: want {want:?}, got {g:?}", c.name)),
                        None => failures.push(
                            format!("{}: field {k} not produced", c.name)),
                    }
                }
            }
            ("ok", Err(e)) => failures.push(
                format!("{}: expected ok, got error {e}", c.name)),
            ("err", Err(e)) => {
                if !c.err_substr.is_empty() && !e.contains(&c.err_substr) {
                    failures.push(format!(
                        "{}: error {e:?} does not contain {:?}", c.name, c.err_substr));
                }
            }
            ("err", Ok(fields)) => failures.push(format!(
                "{}: expected err, got ok with {fields:?}", c.name)),
            (other, _) => failures.push(
                format!("{}: bad expect {other:?}", c.name)),
        }
    }
    if !failures.is_empty() {
        panic!("{} conformance failures:\n{}", failures.len(), failures.join("\n"));
    }
}

#[test]
fn rust_encodes_d2h_cases_as_corpus_says() {
    let cases = load_corpus();
    let mut failures = Vec::new();
    for c in &cases {
        if c.direction != "d2h" || c.expect != "ok" { continue; }
        let encoded = match c.kind.as_str() {
            "ack" => {
                let pts: u64 = c.fields["pts_ns"].parse().unwrap();
                encode_ack(pts).to_vec()
            }
            "touch" => {
                let action = match c.fields["action"].as_str() {
                    "Down" => TouchAction::Down,
                    "Move" => TouchAction::Move,
                    "Up" => TouchAction::Up,
                    "Cancel" => TouchAction::Cancel,
                    other => { failures.push(format!("{}: bad action {other}", c.name)); continue; }
                };
                let x: u16 = c.fields["x"].parse().unwrap();
                let y: u16 = c.fields["y"].parse().unwrap();
                encode_touch(&TouchMessage { action, x, y }).to_vec()
            }
            _ => continue,
        };
        if encoded != c.bytes {
            failures.push(format!(
                "{}: encoded {:02x?} but corpus says {:02x?}",
                c.name, encoded, c.bytes));
        }
    }
    if !failures.is_empty() {
        panic!("{} encode failures:\n{}", failures.len(), failures.join("\n"));
    }
}
