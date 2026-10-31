//! One constant register an instruction, as ps_3_0's single read port
//! wants and Windows' D3D9 enforces; vkd3d's `cmp r0, r1, c0.x, c1.z`
//! breaks it (docs/design.md, item 20). `one_port` moves every constant
//! but the first through a spare temporary; build.rs runs it for the DLL.

const END: u32 = 0x0000_FFFF;
const COMMENT: u32 = 0xFFFE;
const MOV: u32 = 0x01;
// Instructions whose first parameter is a source, not a destination.
const NO_DEST: [u32; 8] = [0x19, 0x1A, 0x1B, 0x26, 0x28, 0x29, 0x2D, 0x60];
// Declarations and definitions: their parameters are not reads.
const NOT_READS: [u32; 4] = [0x1F, 0x2F, 0x30, 0x51];
const TEMP: u32 = 0;
const CONST: u32 = 2;
/// ps_3_0's temporaries.
const TEMPS: u32 = 32;

fn reg_type(p: u32) -> u32 {
    ((p >> 28) & 7) | ((p >> 8) & 0x18)
}

fn reg_num(p: u32) -> u32 {
    p & 0x7FF
}

/// A parameter token as register `n` of type `ty`, the rest (swizzle or
/// mask, modifiers) kept.
fn with_reg(p: u32, ty: u32, n: u32) -> u32 {
    (p & !0x7000_1FFF) | ((ty & 7) << 28) | ((ty & 0x18) << 8) | n
}

/// The instructions of a shader: (start, opcode, parameter range), or
/// None for one that is not SM 2/3 bytecode as this reads it.
fn instructions(code: &[u32]) -> Option<Vec<(usize, u32, std::ops::Range<usize>)>> {
    let mut out = Vec::new();
    let mut i = 1;
    loop {
        let tok = *code.get(i)?;
        if tok == END {
            return Some(out);
        }
        let op = tok & 0xFFFF;
        let len = if op == COMMENT {
            ((tok >> 16) & 0x7FFF) as usize
        } else {
            ((tok >> 24) & 0xF) as usize
        };
        code.get(i + 1..i + 1 + len)?;
        out.push((i, op, i + 1..i + 1 + len));
        i += 1 + len;
    }
}

/// The source parameters of an instruction that read registers (not an
/// address token after a relative one, not the predicate).
fn sources(code: &[u32], tok: u32, op: u32, params: std::ops::Range<usize>) -> Vec<usize> {
    if op == COMMENT || NOT_READS.contains(&op) {
        return Vec::new();
    }
    let mut at = params.start;
    if !NO_DEST.contains(&op) {
        // The destination, and its address token if relative.
        at += 1 + ((code[params.start] >> 13) & 1) as usize;
        // A predicated instruction's predicate register.
        at += ((tok >> 28) & 1) as usize;
    }
    let mut out = Vec::new();
    while at < params.end {
        out.push(at);
        at += 1 + ((code[at] >> 13) & 1) as usize;
    }
    out
}

/// The distinct constant registers each instruction reads, most first.
pub fn most_constants(code: &[u32]) -> usize {
    instructions(code)
        .unwrap_or_default()
        .into_iter()
        .map(|(i, op, p)| {
            let mut cs: Vec<u32> = sources(code, code[i], op, p)
                .into_iter()
                .filter(|&s| reg_type(code[s]) == CONST)
                .map(|s| reg_num(code[s]))
                .collect();
            cs.sort_unstable();
            cs.dedup();
            cs.len()
        })
        .max()
        .unwrap_or(0)
}

/// `code` with no instruction reading two constant registers; None if it
/// is not bytecode as this reads it or has no temporaries to spare.
pub fn one_port(code: &[u32]) -> Option<Vec<u32>> {
    let ins = instructions(code)?;
    // The temporaries used, so the spares come after them.
    let mut spare = 0;
    for (_, op, p) in &ins {
        if *op == COMMENT || NOT_READS.contains(op) {
            continue;
        }
        for t in p.clone() {
            if code[t] >> 31 == 1 && reg_type(code[t]) == TEMP {
                spare = spare.max(reg_num(code[t]) + 1);
            }
        }
    }
    let mut out = vec![code[0]];
    let mut last = 1;
    for (i, op, p) in ins {
        let srcs = sources(code, code[i], op, p.clone());
        let mut seen: Vec<u32> = Vec::new();
        let mut moves = Vec::new();
        let mut spares = 0;
        let mut body: Vec<u32> = code[i..p.end].to_vec();
        for s in srcs {
            let tok = code[s];
            if reg_type(tok) != CONST {
                continue;
            }
            let n = reg_num(tok);
            let k = match seen.iter().position(|&c| c == n) {
                Some(k) => k,
                None => {
                    seen.push(n);
                    seen.len() - 1
                }
            };
            if k == 0 {
                continue;
            }
            let t = spare + k as u32 - 1;
            if t >= TEMPS {
                return None;
            }
            if k > spares {
                spares = k;
                moves.extend([0x0200_0000 | MOV, 0x800F_0000 | t, 0xA0E4_0000 | n]);
            }
            body[s - i] = with_reg(tok, TEMP, t);
        }
        out.extend_from_slice(&code[last..i]);
        out.extend(moves);
        out.extend(body);
        last = p.end;
    }
    out.extend_from_slice(&code[last..]);
    Some(out)
}
