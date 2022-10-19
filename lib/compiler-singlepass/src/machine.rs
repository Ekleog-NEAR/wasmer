use crate::emitter_x64::*;
use smallvec::smallvec;
use smallvec::SmallVec;
use wasmer_compiler::wasmparser::Type as WpType;
use wasmer_compiler::CallingConvention;

const NATIVE_PAGE_SIZE: usize = 4096;

struct MachineStackOffset(usize);

pub(crate) struct Machine {
    used_gprs: u32, // Bitset for the used GPRs, 1 means used
    used_xmms: u32, // Bitset for the used XMMs, 1 means used
    stack_offset: MachineStackOffset,
    save_area_offset: Option<MachineStackOffset>,
    /// Memory location at which local variables begin.
    ///
    /// Populated in `init_locals`.
    locals_offset: MachineStackOffset,
}

/// Returns an u32 that has as 1 bits the ones matching registers passed as parameters
macro_rules! bitset_of_regs {
    ($( $r:expr ),*) => {{
        $( (1u32 << ($r as u32)) )|*
    }}
}

// Note: the below asserts are because we currently use u32 for used_gprs and used_xmms
// Feel free to increase the number in this assert by making them bigger if needed
#[allow(dead_code)]
const _GPRS_FIT_IN_U32: () = assert!(GPR::num_gprs() <= 32);
#[allow(dead_code)]
const _XMMS_FIT_IN_U32: () = assert!(XMM::num_xmms() <= 32);

impl Machine {
    pub(crate) fn new() -> Self {
        Machine {
            used_gprs: 0,
            used_xmms: 0,
            stack_offset: MachineStackOffset(0),
            save_area_offset: None,
            locals_offset: MachineStackOffset(0),
        }
    }

    pub(crate) fn get_stack_offset(&self) -> usize {
        self.stack_offset.0
    }

    fn get_used_in<T>(mut v: u32, to_return_type: impl Fn(u8) -> T) -> Vec<T> {
        let mut n = 0u8;
        let mut res = Vec::with_capacity(v.count_ones() as usize);
        while v != 0 {
            n += v.trailing_zeros() as u8;
            res.push(to_return_type(n));
            v >>= v.trailing_zeros() + 1;
            n += 1;
        }
        res
    }

    pub(crate) fn get_used_gprs(&self) -> Vec<GPR> {
        Self::get_used_in(self.used_gprs, |r| GPR::from_repr(r).unwrap())
    }

    pub(crate) fn get_used_xmms(&self) -> Vec<XMM> {
        Self::get_used_in(self.used_xmms, |r| XMM::from_repr(r).unwrap())
    }

    pub(crate) fn get_vmctx_reg() -> GPR {
        GPR::R15
    }

    fn pick_one_in(v: u32) -> Option<u8> {
        let r = v.trailing_zeros() as u8;
        (r != 32).then_some(r)
    }

    /// Picks an unused general purpose register for local/stack/argument use.
    ///
    /// This method does not mark the register as used.
    pub(crate) fn pick_gpr(&self) -> Option<GPR> {
        use GPR::*;
        const REGS: u32 = bitset_of_regs!(RSI, RDI, R8, R9, R10, R11);
        Self::pick_one_in(!self.used_gprs & REGS).map(|r| GPR::from_repr(r).unwrap())
    }

    /// Picks an unused general purpose register for internal temporary use.
    ///
    /// This method does not mark the register as used.
    pub(crate) fn pick_temp_gpr(&self) -> Option<GPR> {
        use GPR::*;
        const REGS: u32 = bitset_of_regs!(RAX, RCX, RDX);
        Self::pick_one_in(!self.used_gprs & REGS).map(|r| GPR::from_repr(r).unwrap())
    }

    fn get_gpr_used(&self, r: GPR) -> bool {
        if 0 != (self.used_gprs & bitset_of_regs!(r)) {
            true
        } else {
            false
        }
    }

    fn set_gpr_used(&mut self, r: GPR) {
        self.used_gprs |= bitset_of_regs!(r);
    }

    fn set_gpr_unused(&mut self, r: GPR) {
        self.used_gprs &= !bitset_of_regs!(r);
    }

    fn get_xmm_used(&self, r: XMM) -> bool {
        if 0 != (self.used_xmms & bitset_of_regs!(r)) {
            true
        } else {
            false
        }
    }

    fn set_xmm_used(&mut self, r: XMM) {
        self.used_xmms |= bitset_of_regs!(r);
    }

    fn set_xmm_unused(&mut self, r: XMM) {
        self.used_xmms &= !bitset_of_regs!(r);
    }

    /// Acquires a temporary GPR.
    pub(crate) fn acquire_temp_gpr(&mut self) -> Option<GPR> {
        let gpr = self.pick_temp_gpr();
        if let Some(x) = gpr {
            self.set_gpr_used(x);
        }
        gpr
    }

    /// Releases a temporary GPR.
    pub(crate) fn release_temp_gpr(&mut self, gpr: GPR) {
        assert!(self.get_gpr_used(gpr));
        self.set_gpr_unused(gpr);
    }

    /// Specify that a given register is in use.
    pub(crate) fn reserve_unused_temp_gpr(&mut self, gpr: GPR) -> GPR {
        assert!(!self.get_gpr_used(gpr));
        self.set_gpr_used(gpr);
        gpr
    }

    /// Picks an unused XMM register.
    ///
    /// This method does not mark the register as used.
    pub(crate) fn pick_xmm(&self) -> Option<XMM> {
        use XMM::*;
        const REGS: u32 = bitset_of_regs!(XMM3, XMM4, XMM5, XMM6, XMM7);
        Self::pick_one_in(!self.used_xmms & REGS).map(|r| XMM::from_repr(r).unwrap())
    }

    /// Picks an unused XMM register for internal temporary use.
    ///
    /// This method does not mark the register as used.
    pub(crate) fn pick_temp_xmm(&self) -> Option<XMM> {
        use XMM::*;
        const REGS: u32 = bitset_of_regs!(XMM0, XMM1, XMM2);
        Self::pick_one_in(!self.used_xmms & REGS).map(|r| XMM::from_repr(r).unwrap())
    }

    /// Acquires a temporary XMM register.
    pub(crate) fn acquire_temp_xmm(&mut self) -> Option<XMM> {
        let xmm = self.pick_temp_xmm();
        if let Some(x) = xmm {
            self.set_xmm_used(x);
        }
        xmm
    }

    /// Releases a temporary XMM register.
    pub(crate) fn release_temp_xmm(&mut self, xmm: XMM) {
        assert!(self.get_xmm_used(xmm));
        self.set_xmm_unused(xmm);
    }

    /// Acquires locations from the machine state.
    ///
    /// If the returned locations are used for stack value, `release_location` needs to be called on them;
    /// Otherwise, if the returned locations are used for locals, `release_location` does not need to be called on them.
    pub(crate) fn acquire_locations<E: Emitter>(
        &mut self,
        assembler: &mut E,
        tys: &[WpType],
        zeroed: bool,
    ) -> SmallVec<[Location; 1]> {
        let mut ret = smallvec![];
        let mut delta_stack_offset: usize = 0;

        for ty in tys {
            let loc = match *ty {
                WpType::F32 | WpType::F64 => self.pick_xmm().map(Location::XMM),
                WpType::I32 | WpType::I64 => self.pick_gpr().map(Location::GPR),
                WpType::FuncRef | WpType::ExternRef => self.pick_gpr().map(Location::GPR),
                _ => unreachable!("can't acquire location for type {:?}", ty),
            };

            let loc = if let Some(x) = loc {
                x
            } else {
                self.stack_offset.0 += 8;
                delta_stack_offset += 8;
                Location::Memory(GPR::RBP, -(self.stack_offset.0 as i32))
            };
            if let Location::GPR(x) = loc {
                self.set_gpr_used(x);
            } else if let Location::XMM(x) = loc {
                self.set_xmm_used(x);
            }
            ret.push(loc);
        }

        if delta_stack_offset != 0 {
            assembler.emit_sub(
                Size::S64,
                Location::Imm32(delta_stack_offset as u32),
                Location::GPR(GPR::RSP),
            );
        }
        if zeroed {
            for i in 0..tys.len() {
                assembler.emit_mov(Size::S64, Location::Imm32(0), ret[i]);
            }
        }
        ret
    }

    /// Releases locations used for stack value.
    pub(crate) fn release_locations<E: Emitter>(&mut self, assembler: &mut E, locs: &[Location]) {
        let mut delta_stack_offset: usize = 0;

        for loc in locs.iter().rev() {
            match *loc {
                Location::GPR(x) => {
                    assert!(self.get_gpr_used(x));
                    self.set_gpr_unused(x);
                }
                Location::XMM(x) => {
                    assert!(self.get_xmm_used(x));
                    self.set_xmm_unused(x);
                }
                Location::Memory(GPR::RBP, x) => {
                    if x >= 0 {
                        unreachable!();
                    }
                    let offset = (-x) as usize;
                    if offset != self.stack_offset.0 {
                        unreachable!();
                    }
                    self.stack_offset.0 -= 8;
                    delta_stack_offset += 8;
                }
                _ => {}
            }
        }

        if delta_stack_offset != 0 {
            assembler.emit_add(
                Size::S64,
                Location::Imm32(delta_stack_offset as u32),
                Location::GPR(GPR::RSP),
            );
        }
    }

    pub(crate) fn release_locations_only_regs(&mut self, locs: &[Location]) {
        for loc in locs.iter().rev() {
            match *loc {
                Location::GPR(x) => {
                    assert!(self.get_gpr_used(x));
                    self.set_gpr_unused(x);
                }
                Location::XMM(x) => {
                    assert!(self.get_xmm_used(x));
                    self.set_xmm_unused(x);
                }
                _ => {}
            }
        }
    }

    pub(crate) fn release_locations_only_stack<E: Emitter>(
        &mut self,
        assembler: &mut E,
        locs: &[Location],
    ) {
        let mut delta_stack_offset: usize = 0;

        for loc in locs.iter().rev() {
            if let Location::Memory(GPR::RBP, x) = *loc {
                if x >= 0 {
                    unreachable!();
                }
                let offset = (-x) as usize;
                if offset != self.stack_offset.0 {
                    unreachable!();
                }
                self.stack_offset.0 -= 8;
                delta_stack_offset += 8;
            }
        }

        if delta_stack_offset != 0 {
            assembler.emit_add(
                Size::S64,
                Location::Imm32(delta_stack_offset as u32),
                Location::GPR(GPR::RSP),
            );
        }
    }

    pub(crate) fn release_locations_keep_state<E: Emitter>(
        &self,
        assembler: &mut E,
        locs: &[Location],
    ) {
        let mut delta_stack_offset: usize = 0;
        let mut stack_offset = self.stack_offset.0;

        for loc in locs.iter().rev() {
            if let Location::Memory(GPR::RBP, x) = *loc {
                if x >= 0 {
                    unreachable!();
                }
                let offset = (-x) as usize;
                if offset != stack_offset {
                    unreachable!();
                }
                stack_offset -= 8;
                delta_stack_offset += 8;
            }
        }

        if delta_stack_offset != 0 {
            assembler.emit_add(
                Size::S64,
                Location::Imm32(delta_stack_offset as u32),
                Location::GPR(GPR::RSP),
            );
        }
    }

    const LOCAL_REGISTERS: &'static [GPR] = &[GPR::R12, GPR::R13, GPR::R14, GPR::RBX];

    pub(crate) fn get_local_location(&self, idx: u32) -> Location {
        // NB: This calculation cannot reasonably overflow. `self.locals_offset` will typically be
        // small (< 32), and `idx` is bounded to `51000` due to limits imposed by the wasmparser
        // validator. We introduce a debug_assert here to ensure that `idx` never really exceeds
        // some incredibly large value.
        debug_assert!(
            idx <= 999_999,
            "this runtime can't deal with unreasonable number of locals"
        );
        Self::LOCAL_REGISTERS
            .get(idx as usize)
            .map(|r| Location::GPR(*r))
            .unwrap_or_else(|| {
                let local_offset = idx
                    .checked_sub(Self::LOCAL_REGISTERS.len() as u32)
                    .unwrap()
                    .wrapping_mul(8);
                Location::Memory(
                    GPR::RBP,
                    (local_offset.wrapping_add(self.locals_offset.0 as u32) as i32).wrapping_neg(),
                )
            })
    }

    pub(crate) fn init_locals<E: Emitter>(
        &mut self,
        a: &mut E,
        n: u32,
        n_params: u32,
        calling_convention: CallingConvention,
    ) {
        // Total size (in bytes) of the pre-allocated "static area" for this function's
        // locals and callee-saved registers.
        let mut static_area_size: usize = 0;

        // Space to clobber registers used for locals.
        static_area_size += 8 * std::cmp::min(Self::LOCAL_REGISTERS.len(), n as usize);

        // Callee-saved R15 for vmctx.
        static_area_size += 8;

        // For Windows ABI, save RDI and RSI
        if calling_convention == CallingConvention::WindowsFastcall {
            static_area_size += 8 * 2;
        }

        // The offset pointing at the very first local. Right now `static_area_size` is pointing at
        // the end address of the 0th local, not at the start address, so we add `8` bytes to fix
        // this up.
        self.locals_offset = MachineStackOffset(static_area_size + 8);
        let locals_size = (n as usize).saturating_sub(Self::LOCAL_REGISTERS.len()) * 8;

        // Allocate the stack, without actually writing to it.
        a.emit_sub(
            Size::S64,
            Location::Imm32((static_area_size + locals_size) as _),
            Location::GPR(GPR::RSP),
        );

        // Save callee-saved registers
        for local_reg in Self::LOCAL_REGISTERS.iter().take(n as usize) {
            self.stack_offset.0 += 8;
            a.emit_mov(
                Size::S64,
                Location::GPR(*local_reg),
                Location::Memory(GPR::RBP, -(self.stack_offset.0 as i32)),
            );
        }

        // Save R15 for vmctx use.
        self.stack_offset.0 += 8;
        a.emit_mov(
            Size::S64,
            Location::GPR(GPR::R15),
            Location::Memory(GPR::RBP, -(self.stack_offset.0 as i32)),
        );

        if calling_convention == CallingConvention::WindowsFastcall {
            for reg in [GPR::RDI, GPR::RSI] {
                self.stack_offset.0 += 8;
                a.emit_mov(
                    Size::S64,
                    Location::GPR(reg),
                    Location::Memory(GPR::RBP, -(self.stack_offset.0 as i32)),
                );
            }
        }

        // Save the offset of register save area.
        self.save_area_offset = Some(MachineStackOffset(self.stack_offset.0));

        // Load in-register parameters into the allocated locations.
        // Locals are allocated on the stack from higher address to lower address,
        // so we won't skip the stack guard page here.
        for i in 0..n_params {
            // NB: the 0th parameter is used for passing around the internal VM data (vmctx).
            let loc = Self::get_param_location((i + 1) as usize, calling_convention);
            let local_loc = self.get_local_location(i);
            match loc {
                Location::GPR(_) => {
                    a.emit_mov(Size::S64, loc, local_loc);
                }
                Location::Memory(_, _) => match local_loc {
                    Location::GPR(_) => {
                        a.emit_mov(Size::S64, loc, local_loc);
                    }
                    Location::Memory(_, _) => {
                        a.emit_mov(Size::S64, loc, Location::GPR(GPR::RAX));
                        a.emit_mov(Size::S64, Location::GPR(GPR::RAX), local_loc);
                    }
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            }
        }

        // Load vmctx into R15.
        a.emit_mov(
            Size::S64,
            Self::get_param_location(0, calling_convention),
            Location::GPR(GPR::R15),
        );

        // Stack probe.
        //
        // `rep stosq` writes data from low address to high address and may skip the stack guard page.
        // so here we probe it explicitly when needed.
        for i in (n_params..n).step_by(NATIVE_PAGE_SIZE / 8).skip(1) {
            a.emit_mov(Size::S64, Location::Imm32(0), self.get_local_location(i));
        }

        // Initialize all remaining locals to zero.
        //
        // This is a little tricky, as we want to initialize all stack local slots, except for
        // those that were already populated with function argument data. The complication is in
        // the fact that we allocate some registers to the first couple local slots.
        //
        // First: handle the locals that are allocated to registers...
        for local_reg_idx in Self::LOCAL_REGISTERS
            .iter()
            .skip(n_params as usize)
            .take((n_params..n).len())
        {
            a.emit_mov(Size::S64, Location::Imm32(0), Location::GPR(*local_reg_idx));
        }
        // Second: handle the locals that are allocated to the stack.
        let stack_loc_idxs = std::cmp::max(Self::LOCAL_REGISTERS.len() as u32, n_params)..n;
        if stack_loc_idxs.len() > 0 {
            // Since these assemblies take up to 24 bytes, if more than 2 slots are initialized, then they are smaller.
            a.emit_mov(
                Size::S64,
                Location::Imm64(stack_loc_idxs.len() as u64),
                Location::GPR(GPR::RCX),
            );
            a.emit_xor(Size::S64, Location::GPR(GPR::RAX), Location::GPR(GPR::RAX));
            a.emit_lea(
                Size::S64,
                self.get_local_location(n - 1),
                Location::GPR(GPR::RDI),
            );
            a.emit_rep_stosq();
        }

        // Add the size of all locals allocated to stack.
        self.stack_offset.0 += locals_size;
    }

    pub(crate) fn finalize_locals<E: Emitter>(
        &mut self,
        a: &mut E,
        calling_convention: CallingConvention,
        local_count: u32,
    ) {
        // Unwind stack to the "save area".
        a.emit_lea(
            Size::S64,
            Location::Memory(
                GPR::RBP,
                -(self.save_area_offset.as_ref().unwrap().0 as i32),
            ),
            Location::GPR(GPR::RSP),
        );

        if calling_convention == CallingConvention::WindowsFastcall {
            // Restore RSI and RDI
            a.emit_pop(Size::S64, Location::GPR(GPR::RSI));
            a.emit_pop(Size::S64, Location::GPR(GPR::RDI));
        }
        // Restore R15 used by vmctx.
        a.emit_pop(Size::S64, Location::GPR(GPR::R15));

        // Restore callee-saved registers that we used for locals.
        for reg in Self::LOCAL_REGISTERS
            .iter()
            .take(local_count as usize)
            .rev()
        {
            a.emit_pop(Size::S64, Location::GPR(*reg));
        }
    }

    pub(crate) fn get_param_location(
        idx: usize,
        calling_convention: CallingConvention,
    ) -> Location {
        match calling_convention {
            CallingConvention::WindowsFastcall => match idx {
                0 => Location::GPR(GPR::RCX),
                1 => Location::GPR(GPR::RDX),
                2 => Location::GPR(GPR::R8),
                3 => Location::GPR(GPR::R9),
                _ => Location::Memory(GPR::RBP, (16 + 32 + (idx - 4) * 8) as i32),
            },
            _ => match idx {
                0 => Location::GPR(GPR::RDI),
                1 => Location::GPR(GPR::RSI),
                2 => Location::GPR(GPR::RDX),
                3 => Location::GPR(GPR::RCX),
                4 => Location::GPR(GPR::R8),
                5 => Location::GPR(GPR::R9),
                _ => Location::Memory(GPR::RBP, (16 + (idx - 6) * 8) as i32),
            },
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use dynasmrt::x64::X64Relocation;
    use dynasmrt::VecAssembler;
    type Assembler = VecAssembler<X64Relocation>;

    #[test]
    fn test_release_locations_keep_state_nopanic() {
        let mut machine = Machine::new();
        let mut assembler = Assembler::new(0);
        let locs = machine.acquire_locations(
            &mut assembler,
            &(0..10).map(|_| WpType::I32).collect::<Vec<_>>(),
            false,
        );

        machine.release_locations_keep_state(&mut assembler, &locs);
    }
}
