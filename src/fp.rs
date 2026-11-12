//! Address-independent function fingerprints. The body is decoded with iced-x86 and hashed
//! as a stream of (opcode, operand shapes): registers and small constants are kept, branch
//! targets, RIP-relative displacements and image addresses are masked, so a function whose
//! code did not change hashes equal in two builds even though everything around it moved.
//!
//! Two hashes are produced: `exact` (everything above) and `coarse` (opcodes and operand kinds
//! only), the latter for matching functions whose constants or registers changed.
//!
//! Function boundaries come from the image's own tables (`.eh_frame` FDEs in ELF, `.pdata` in
//! PE) when the binary records the function; otherwise the body is bounded by the next known
//! function start and cut at a `ret` followed by padding, which is a heuristic.

use crate::image::{Image, Target};
use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use std::collections::HashMap;
use xxhash_rust::xxh3::xxh3_64;

/// Longest body we hash; a function is otherwise bounded by the next known function start.
const CAP: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fp {
    pub exact: u64,
    pub coarse: u64,
    pub instructions: u32,
}

const NULL_FP: Fp = Fp {
    exact: 0x4E55_4C4C,
    coarse: 0x4E55_4C4C,
    instructions: 0,
};
const EMPTY_FP: Fp = Fp {
    exact: 0,
    coarse: 0,
    instructions: 0,
};

pub struct Fingerprinter<'a> {
    img: &'a Image,
    starts: &'a [u64],
    /// The start list is complete enough to bound a function by the next start (the signature
    /// module's inventory); trailing padding is then left out of the hash.
    trust_starts: bool,
    cache: HashMap<u64, Fp>,
}

impl<'a> Fingerprinter<'a> {
    pub fn new(img: &'a Image, starts: &'a [u64]) -> Self {
        Fingerprinter {
            img,
            starts,
            trust_starts: false,
            cache: HashMap::new(),
        }
    }

    /// For a start list that holds every function the image lets us see: a function without a
    /// recorded range ends at the next start, not at the first `ret` that looks final.
    pub fn with_trusted_starts(img: &'a Image, starts: &'a [u64]) -> Self {
        Fingerprinter {
            img,
            starts,
            trust_starts: true,
            cache: HashMap::new(),
        }
    }

    /// How many functions have an exact range from the image's tables rather than the heuristic.
    pub fn fde_count(&self) -> usize {
        self.img.func_ranges.len()
    }

    pub fn of(&mut self, t: &Target) -> Fp {
        match t {
            Target::Sym(s) => {
                let h = xxh3_64(s.as_bytes()) | 1;
                Fp {
                    exact: h,
                    coarse: h,
                    instructions: 0,
                }
            }
            Target::Null => NULL_FP,
            Target::Addr(a) => {
                if let Some(&h) = self.cache.get(a) {
                    return h;
                }
                let h = self.compute(*a);
                self.cache.insert(*a, h);
                h
            }
        }
    }

    fn compute(&self, v: u64) -> Fp {
        let (bound, exact_end) = match self.img.func_ranges.get(&v) {
            Some(&end) => (end.min(self.img.text.hi), true),
            None => {
                let i = self.starts.partition_point(|&s| s <= v);
                let next = if i < self.starts.len() {
                    self.starts[i]
                } else {
                    self.img.text.hi
                };
                if self.trust_starts {
                    (next.min(v + 4 * CAP).min(self.img.text.hi), true)
                } else {
                    (next.min(v + CAP).min(self.img.text.hi), false)
                }
            }
        };
        if bound <= v {
            return EMPTY_FP;
        }
        let Some(body) = self.img.slice(&self.img.text, v, (bound - v) as usize) else {
            return EMPTY_FP;
        };

        let mut dec = Decoder::with_ip(64, body, v, DecoderOptions::NONE);
        let mut ins = Instruction::default();
        let mut exact: Vec<u8> = Vec::with_capacity(body.len() * 2);
        let mut coarse: Vec<u8> = Vec::with_capacity(body.len());
        let mut count = 0u32;
        let mut pad_mark: Option<(usize, usize, u32)> = None;
        fn push(buf: &mut Vec<u8>, x: u64) {
            buf.extend_from_slice(&x.to_le_bytes());
        }

        while dec.can_decode() {
            dec.decode_out(&mut ins);
            if ins.is_invalid() {
                break;
            }
            if self.trust_starts {
                let pad = matches!(ins.mnemonic(), Mnemonic::Nop | Mnemonic::Int3);
                if !pad {
                    pad_mark = None;
                } else if pad_mark.is_none() {
                    pad_mark = Some((exact.len(), coarse.len(), count));
                }
            }
            count += 1;
            push(&mut exact, ins.code() as u32 as u64);
            push(&mut coarse, ins.code() as u32 as u64);
            for k in 0..ins.op_count() {
                let kind = ins.op_kind(k);
                push(&mut coarse, kind as u32 as u64);
                match kind {
                    OpKind::Register => {
                        push(&mut exact, 0x1_0000 + ins.op_register(k) as u32 as u64)
                    }
                    OpKind::NearBranch16
                    | OpKind::NearBranch32
                    | OpKind::NearBranch64
                    | OpKind::FarBranch16
                    | OpKind::FarBranch32 => push(&mut exact, 0xB0B0),
                    OpKind::Memory => {
                        push(&mut exact, 0x2_0000 + ins.memory_base() as u32 as u64);
                        push(&mut exact, 0x3_0000 + ins.memory_index() as u32 as u64);
                        push(&mut exact, ins.memory_index_scale() as u64);
                        push(&mut exact, 0x4_0000 + ins.memory_size() as u32 as u64);
                        let absolute = ins.memory_base() == Register::None
                            && ins.memory_index() == Register::None;
                        if ins.is_ip_rel_memory_operand() || absolute {
                            push(&mut exact, 0xD15F);
                        } else {
                            push(&mut exact, ins.memory_displacement64());
                        }
                    }
                    OpKind::Immediate8
                    | OpKind::Immediate8_2nd
                    | OpKind::Immediate16
                    | OpKind::Immediate32
                    | OpKind::Immediate64
                    | OpKind::Immediate8to16
                    | OpKind::Immediate8to32
                    | OpKind::Immediate8to64
                    | OpKind::Immediate32to64 => {
                        let imm = ins.immediate(k);
                        push(
                            &mut exact,
                            if self.img.addr_like(imm) { 0x1111 } else { imm },
                        );
                    }
                    _ => push(&mut exact, 0xEEEE),
                }
            }
            if !exact_end {
                let fc = ins.flow_control();
                if fc == FlowControl::Return || fc == FlowControl::UnconditionalBranch {
                    let next_ip = ins.next_ip();
                    if next_ip >= bound {
                        break;
                    }
                    // heuristic end: padding after a ret (int3 or a nop form), or int3 after a
                    // tail jump (MSVC separates functions with int3 and emits none inside one; a
                    // jmp followed by a nop form may be a loop head aligned inside the function,
                    // so that one does not count). The jump's target is not consulted: two tiny
                    // adjacent functions tail-jumping to their callees must not absorb each other.
                    let b = body[(next_ip - v) as usize];
                    let padded = if fc == FlowControl::Return {
                        b == 0xCC || b == 0x90 || b == 0x0F || b == 0x66
                    } else {
                        b == 0xCC
                    };
                    if padded {
                        break;
                    }
                }
            }
        }
        if let Some((e, c, n)) = pad_mark {
            exact.truncate(e);
            coarse.truncate(c);
            count = n;
        }
        Fp {
            exact: xxh3_64(&exact),
            coarse: xxh3_64(&coarse),
            instructions: count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{Format, Section};

    /// A one-section image whose .text holds `code` at 0x1000, no function ranges: the heuristic
    /// end is exercised alone.
    fn image(code: &[u8]) -> Image {
        let text = Section {
            lo: 0x1000,
            hi: 0x1000 + code.len() as u64,
            off: 0,
        };
        Image {
            path: "synthetic".into(),
            version: "?".into(),
            format: Format::Elf,
            base: 0,
            data: code.to_vec(),
            text,
            drr: text,
            rodata: vec![],
            relocs: HashMap::new(),
            func_ranges: HashMap::new(),
            func_syms: Default::default(),
            imports: HashMap::new(),
            img_lo: 0x1000,
            img_hi: 0x1000 + code.len() as u64,
        }
    }

    #[test]
    fn a_tail_jump_out_of_the_body_followed_by_int3_ends_the_function() {
        // 1000: mov ecx,edx ; jmp +0x50 (to 0x1057, past the bound) ; int3 ; then another function
        let code = [
            0x8B, 0xCA, 0xE9, 0x50, 0x00, 0x00, 0x00, 0xCC, 0x8D, 0x42, 0x1D, 0xC3, 0xCC,
        ];
        let img = image(&code);
        let starts = [0x1000u64];
        let mut f = Fingerprinter::new(&img, &starts);
        assert_eq!(
            f.of(&Target::Addr(0x1000)).instructions,
            2,
            "mov + jmp, nothing after the int3"
        );
    }

    #[test]
    fn a_jump_followed_by_int3_is_taken_as_a_tail_call_even_when_it_lands_nearby() {
        // 1000: jmp +1 (to 0x1003) ; int3 ; 1003: lea eax,[rdx+0x1d] ; ret ; int3
        // The documented limit of the heuristic: a jump over an int3 inside one function would be
        // cut here. Compilers do not emit int3 inside a function (MSVC pads between them, GCC uses
        // ud2 for traps), and a function with unwind information never reaches this code path.
        // Consulting the target instead broke two adjacent tiny functions tail-jumping to their
        // callees (the Twins fixture under MSVC), which is the real pattern.
        let code = [0xEB, 0x01, 0xCC, 0x8D, 0x42, 0x1D, 0xC3, 0xCC];
        let img = image(&code);
        let starts = [0x1000u64];
        let mut f = Fingerprinter::new(&img, &starts);
        assert_eq!(f.of(&Target::Addr(0x1000)).instructions, 1);
    }

    #[test]
    fn ret_followed_by_padding_ends_the_function() {
        let code = [0x8D, 0x42, 0x1D, 0xC3, 0x90, 0x90, 0xC3];
        let img = image(&code);
        let starts = [0x1000u64];
        let mut f = Fingerprinter::new(&img, &starts);
        assert_eq!(f.of(&Target::Addr(0x1000)).instructions, 2, "lea + ret");
    }
}
