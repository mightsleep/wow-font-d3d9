//! ports: vkd3d's decoder with one constant register an instruction, and
//! nothing else changed.

use crate::ports::{most_constants, one_port};

fn decoder() -> Vec<u32> {
    std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../shaders/decoder.ps3"))
        .unwrap()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&c| u32::from_le_bytes(c))
        .collect()
}

#[test]
fn one_constant_an_instruction() {
    let code = decoder();
    assert!(most_constants(&code) > 1, "vkd3d keeps to the port now: ports.rs can go");
    let fixed = one_port(&code).unwrap();
    assert_eq!(most_constants(&fixed), 1);
    // Again, nothing to do.
    assert_eq!(one_port(&fixed).unwrap(), fixed);
}

/// A hand-made shader: cmp r0, r0, c0, c1 becomes mov r1, c1; cmp r0, r0,
/// c0, r1, the swizzle and the modifiers kept.
#[test]
fn moves_the_second_constant() {
    let code = vec![
        0xFFFF_0300,
        0x0200_0001, 0x800F_0000, 0xA0E4_0000, // mov r0, c0
        0x0400_0058, 0x800F_0000, 0x80E4_0000, 0xA0E4_0000, 0xA1AA_0001, // cmp r0, r0, c0, -c1.zzzz
        0x0200_0001, 0x800F_0800, 0x80E4_0000, // mov oC0, r0
        0x0000_FFFF,
    ];
    let fixed = one_port(&code).unwrap();
    assert_eq!(
        fixed,
        vec![
            0xFFFF_0300,
            0x0200_0001, 0x800F_0000, 0xA0E4_0000,
            0x0200_0001, 0x800F_0001, 0xA0E4_0001, // mov r1, c1
            0x0400_0058, 0x800F_0000, 0x80E4_0000, 0xA0E4_0000, 0x81AA_0001, // cmp r0, r0, c0, -r1.zzzz
            0x0200_0001, 0x800F_0800, 0x80E4_0000,
            0x0000_FFFF,
        ]
    );
}
