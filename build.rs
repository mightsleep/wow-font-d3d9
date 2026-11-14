// WOW_FONT_ENCODER for cache.rs: an Fx hash of the sources of v2's cover
// and words, so cells cached by an older encoder are left behind. And the
// decoder as the DLL embeds it: vkd3d's shaders/decoder.ps3 with one
// constant an instruction (src/ports.rs), in OUT_DIR.

#[allow(dead_code)]
#[path = "src/ports.rs"]
mod ports;

const ENCODER: [&str; 4] = [
    "src/glyph.rs",
    "src/coverage.rs",
    "src/atlas.rs",
    "src/union.rs",
];

fn main() {
    let mut h: u64 = 0;
    for f in ENCODER {
        println!("cargo:rerun-if-changed={}", f);
        for c in std::fs::read(f).expect("encoder source").chunks(8) {
            let mut b = [0u8; 8];
            b[..c.len()].copy_from_slice(c);
            h = (h.rotate_left(5) ^ u64::from_le_bytes(b)).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
    println!("cargo:rustc-env=WOW_FONT_ENCODER={:016x}", h);

    // The commit, for the log's first line: a build between two releases
    // has the last one's version. Given by the flake (the sandbox has no
    // git), else asked of git, else unknown.
    println!("cargo:rerun-if-env-changed=WOW_FONT_D3D9_REV");
    println!("cargo:rerun-if-changed=.git/HEAD");
    let rev = std::env::var("WOW_FONT_D3D9_REV")
        .ok()
        .filter(|r| !r.is_empty())
        .or_else(rev_of_git)
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=WOW_FONT_D3D9_REV={}", rev);

    println!("cargo:rerun-if-changed=shaders/decoder.ps3");
    println!("cargo:rerun-if-changed=src/ports.rs");
    let code: Vec<u32> = std::fs::read("shaders/decoder.ps3")
        .expect("shaders/decoder.ps3")
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&c| u32::from_le_bytes(c))
        .collect();
    let fixed = ports::one_port(&code).expect("decoder.ps3: no temporaries to spare");
    assert_eq!(
        ports::most_constants(&fixed),
        1,
        "decoder.ps3: still two constants an instruction"
    );
    let bytes: Vec<u8> = fixed.iter().flat_map(|w| w.to_le_bytes()).collect();
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("decoder.ps3");
    std::fs::write(out, bytes).expect("OUT_DIR/decoder.ps3");
}

/// HEAD's short hash, with -dirty if the tree differs from it.
fn rev_of_git() -> Option<String> {
    let git = |args: &[&str]| {
        let o = std::process::Command::new("git").args(args).output().ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let rev = git(&["rev-parse", "--short=7", "HEAD"]).filter(|r| !r.is_empty())?;
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    Some(if dirty { format!("{rev}-dirty") } else { rev })
}
