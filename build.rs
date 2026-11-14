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

    // The DLL's mark, where a loaded copy of it can be told by (src/below.rs):
    // the image version in the PE header, 0xD3D9 and the crate's version
    // in four, eight and four bits. Nothing else reads the field.
    let part = |v: &str| -> u32 {
        std::env::var(v)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    };
    let minor = (part("CARGO_PKG_VERSION_MAJOR") & 0xf) << 12
        | (part("CARGO_PKG_VERSION_MINOR") & 0xff) << 4
        | (part("CARGO_PKG_VERSION_PATCH") & 0xf);
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-cdylib-link-arg=/VERSION:{}.{}", 0xD3D9, minor);
    } else {
        println!(
            "cargo:rustc-cdylib-link-arg=-Wl,--major-image-version,{}",
            0xD3D9
        );
        println!(
            "cargo:rustc-cdylib-link-arg=-Wl,--minor-image-version,{}",
            minor
        );
    }

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
