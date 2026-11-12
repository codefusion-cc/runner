//! The seccomp filter a container's supervisor installs before its job: no user namespace of the job's own.
//!
//! A user namespace gives its maker every capability inside it, and with them the kernel's code a capability
//! guards: nf_tables, mounts of the file systems a user namespace may mount, and the rest. In a machine that is the
//! guest kernel's surface; in a container it is the host's. Engines differ in what their default profile lets
//! through (Podman's lets `unshare` make one), so the supervisor installs its own, which stacks on the engine's.
//!
//! The filter refuses `unshare` and `clone` with `CLONE_NEWUSER` (`EPERM`), `clone3`, whose flags are behind a
//! pointer a filter cannot read (`ENOSYS`, so the C library falls back to `clone`), every x32 call on x86-64, and
//! every call of an architecture it does not know. It is a classic BPF program ([`no_user_namespaces`]); [`verdict`]
//! runs one as the kernel does, for the tests.

/// `AUDIT_ARCH_X86_64`.
pub const X86_64: u32 = 0xc000_003e;
/// `AUDIT_ARCH_I386`: 32-bit programs on an x86-64 kernel.
pub const I386: u32 = 0x4000_0003;
/// `AUDIT_ARCH_AARCH64`.
pub const AARCH64: u32 = 0xc000_00b7;
/// `AUDIT_ARCH_ARM`: 32-bit programs on an arm64 kernel.
pub const ARM: u32 = 0x4000_0028;
/// The bit of an x32 call's number on x86-64 (`__X32_SYSCALL_BIT`).
pub const X32_BIT: u32 = 0x4000_0000;
/// `CLONE_NEWUSER`.
pub const CLONE_NEWUSER: u64 = 0x1000_0000;

const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const EPERM: u32 = 1;
const ENOSYS: u32 = 38;

const LD_W_ABS: u16 = 0x20;
const JEQ_K: u16 = 0x15;
const JSET_K: u16 = 0x45;
const RET_K: u16 = 0x06;

/// Where `seccomp_data` keeps the call's number, its architecture and the low half of its first argument (on the
/// little-endian architectures above).
const NR: u32 = 0;
const ARCH: u32 = 4;
const ARG0_LOW: u32 = 16;

/// One instruction of a classic BPF program: the kernel's `struct sock_filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// What the kernel gives a filter of a call: its `struct seccomp_data`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Data {
    pub nr: i32,
    pub arch: u32,
    pub ip: u64,
    pub args: [u64; 6],
}

/// The filter's answer that lets a call through.
pub fn allow() -> u32 {
    RET_ALLOW
}

/// The calls of one architecture a user namespace is made with.
struct Calls {
    arch: u32,
    unshare: u32,
    clone: u32,
    clone3: u32,
}

const ARCHES: [Calls; 4] = [
    Calls {
        arch: X86_64,
        unshare: 272,
        clone: 56,
        clone3: 435,
    },
    Calls {
        arch: I386,
        unshare: 310,
        clone: 120,
        clone3: 435,
    },
    Calls {
        arch: AARCH64,
        unshare: 97,
        clone: 220,
        clone3: 435,
    },
    Calls {
        arch: ARM,
        unshare: 337,
        clone: 120,
        clone3: 435,
    },
];

/// A jump's target: the next instruction or a label.
#[derive(Clone, Copy)]
enum To {
    Next,
    At(&'static str),
}

enum Asm {
    Label(&'static str),
    Load(u32),
    Jeq(u32, To, To),
    Jset(u32, To, To),
    Ret(u32),
}

/// The program: per architecture its calls, then the shared check of the flags and the answers.
pub fn no_user_namespaces() -> Vec<Insn> {
    use Asm::{Jeq, Jset, Label, Load, Ret};
    use To::{At, Next};
    let skip = ["not-x86-64", "not-i386", "not-aarch64", "not-arm"];
    let mut asm = vec![Load(ARCH)];
    for (calls, skip) in ARCHES.iter().zip(skip) {
        asm.push(Jeq(calls.arch, Next, At(skip)));
        asm.push(Load(NR));
        if calls.arch == X86_64 {
            asm.push(Jset(X32_BIT, At("deny"), Next));
        }
        asm.push(Jeq(calls.unshare, At("flags"), Next));
        asm.push(Jeq(calls.clone, At("flags"), Next));
        asm.push(Jeq(calls.clone3, At("no-such-call"), Next));
        asm.push(Ret(RET_ALLOW));
        // The architecture is still what is loaded for the next one's check.
        asm.push(Label(skip));
    }
    asm.extend([
        // An architecture the filter does not know.
        Ret(RET_ERRNO | EPERM),
        Label("flags"),
        Load(ARG0_LOW),
        Jset(CLONE_NEWUSER as u32, At("deny"), At("allow")),
        Label("deny"),
        Ret(RET_ERRNO | EPERM),
        Label("no-such-call"),
        Ret(RET_ERRNO | ENOSYS),
        Label("allow"),
        Ret(RET_ALLOW),
    ]);
    assemble(&asm)
}

fn assemble(asm: &[Asm]) -> Vec<Insn> {
    let mut at = std::collections::HashMap::new();
    let mut next = 0usize;
    for op in asm {
        match op {
            Asm::Label(name) => {
                at.insert(*name, next);
            }
            _ => next += 1,
        }
    }
    let mut program = Vec::with_capacity(next);
    let jump = |from: usize, to: To| -> u8 {
        let target = match to {
            To::Next => from + 1,
            To::At(name) => at[name],
        };
        u8::try_from(target - (from + 1)).expect("a jump within a byte, forward")
    };
    for op in asm {
        let here = program.len();
        let (code, jt, jf, k) = match *op {
            Asm::Label(_) => continue,
            Asm::Load(offset) => (LD_W_ABS, 0, 0, offset),
            Asm::Jeq(k, yes, no) => (JEQ_K, jump(here, yes), jump(here, no), k),
            Asm::Jset(k, yes, no) => (JSET_K, jump(here, yes), jump(here, no), k),
            Asm::Ret(k) => (RET_K, 0, 0, k),
        };
        program.push(Insn { code, jt, jf, k });
    }
    program
}

/// What the kernel answers a call with under `program`: the instructions above run as the kernel runs them. An
/// instruction it does not know, a load out of `seccomp_data` or a run off the end kills, as the kernel's checker
/// would have refused the program.
pub fn verdict(program: &[Insn], data: &Data) -> u32 {
    let mut bytes = [0u8; 64];
    bytes[0..4].copy_from_slice(&data.nr.to_le_bytes());
    bytes[4..8].copy_from_slice(&data.arch.to_le_bytes());
    bytes[8..16].copy_from_slice(&data.ip.to_le_bytes());
    for (index, arg) in data.args.iter().enumerate() {
        bytes[16 + index * 8..24 + index * 8].copy_from_slice(&arg.to_le_bytes());
    }
    let (mut pc, mut a) = (0usize, 0u32);
    while let Some(insn) = program.get(pc) {
        let k = insn.k;
        let branch = |taken: bool| pc + 1 + usize::from(if taken { insn.jt } else { insn.jf });
        pc = match insn.code {
            LD_W_ABS => {
                let at = k as usize;
                let Some(word) = bytes.get(at..at + 4).filter(|_| at.is_multiple_of(4)) else {
                    return 0;
                };
                a = u32::from_le_bytes(word.try_into().unwrap_or_default());
                pc + 1
            }
            JEQ_K => branch(a == k),
            JSET_K => branch(a & k != 0),
            RET_K => return k,
            _ => return 0,
        };
    }
    0
}
