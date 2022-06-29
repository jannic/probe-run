use std::time::Instant;

use probe_rs::{Core, CoreRegisterAddress, MemoryInterface, Session};

use crate::{registers::PC, Elf, TargetInfo, TIMEOUT};

const CANARY_VALUE: u8 = 0xAA;

/// (Location of) the stack canary
///
/// The stack canary is used to detect *potential* stack overflows
///
/// The canary is placed in memory as shown in the diagram below:
///
/// ``` text
/// +--------+ -> initial_stack_pointer / stack_range.end()
/// |        |
/// | stack  | (grows downwards)
/// |        |
/// +--------+
/// |        |
/// |        |
/// +--------+
/// | canary |
/// +--------+ -> stack_range.start()
/// |        |
/// | static | (variables, fixed size)
/// |        |
/// +--------+ -> lowest RAM address
/// ```
///
/// The whole canary is initialized to `CANARY_VALUE` before the target program is started.
/// The canary size is 10% of the available stack space or 1 KiB, whichever is smallest.
///
/// When the programs ends (due to panic or breakpoint) the integrity of the canary is checked. If it was
/// "touched" (any of its bytes != `CANARY_VALUE`) then that is considered to be a *potential* stack
/// overflow.
#[derive(Clone, Copy)]
pub(crate) struct Canary {
    address: u32,
    size: usize,
    stack_available: u32,
    data_below_stack: bool,
    measure_stack: bool,
}

impl Canary {
    /// Decide if and where to place the stack canary.
    pub(crate) fn install(
        sess: &mut Session,
        target_info: &TargetInfo,
        elf: &Elf,
        measure_stack: bool,
    ) -> Result<Option<Self>, anyhow::Error> {
        let mut core = sess.core(0)?;
        core.reset_and_halt(TIMEOUT)?;

        let stack_info = match &target_info.stack_info {
            Some(stack_info) => stack_info,
            None => {
                log::debug!("couldn't find valid stack range, not placing stack canary");
                return Ok(None);
            }
        };

        if elf.program_uses_heap() {
            log::debug!("heap in use, not placing stack canary");
            return Ok(None);
        }

        let stack_start = *stack_info.range.start();
        let stack_available = *stack_info.range.end() - stack_start;

        let size = if measure_stack {
            // When measuring stack consumption, we have to color the whole stack.
            stack_available as usize
        } else {
            // We consider >90% stack usage a potential stack overflow, but don't go beyond 1 kb
            // since filling a lot of RAM is slow (and 1 kb should be "good enough" for what we're
            // doing).
            round_up(1024.min(stack_available / 10), 4) as usize
        };

        log::debug!(
            "{} bytes of stack available ({:#010X} ..= {:#010X}), using {} byte canary",
            stack_available,
            stack_info.range.start(),
            stack_info.range.end(),
            size,
        );

        let size_kb = size as f64 / 1024.0;
        if measure_stack {
            // Painting 100KB or more takes a few seconds, so provide user feedback.
            log::info!(
                "painting {:.2} KiB of RAM for stack usage estimation",
                size_kb
            );
        }
        let start = Instant::now();
        paint_stack(&mut core, stack_start, size as u32)?;
        let seconds = start.elapsed().as_secs_f64();
        log::trace!(
            "setting up canary took {:.3}s ({:.2} KiB/s)",
            seconds,
            size_kb / seconds
        );

        Ok(Some(Canary {
            address: stack_start,
            size,
            stack_available,
            data_below_stack: stack_info.data_below_stack,
            measure_stack,
        }))
    }

    pub(crate) fn touched(self, mut core: &mut probe_rs::Core, elf: &Elf) -> anyhow::Result<bool> {
        let size_kb = self.size as f64 / 1024.0;
        if self.measure_stack {
            log::info!(
                "reading {:.2} KiB of RAM for stack usage estimation",
                size_kb,
            );
        }

        let start = Instant::now();
        let touched_address = measure_stack(&mut core, self.address, self.size as u32)?;
        let seconds = start.elapsed().as_secs_f64();
        log::trace!(
            "reading canary took {:.3}s ({:.2} KiB/s)",
            seconds,
            size_kb / seconds
        );

        let min_stack_usage = match touched_address {
            Some(touched_address) => {
                log::debug!("canary was touched at {:#010X}", touched_address);
                Some(elf.vector_table.initial_stack_pointer - touched_address)
            }
            None => None,
        };

        if self.measure_stack {
            let min_stack_usage = min_stack_usage.unwrap_or(0);
            let used_kb = min_stack_usage as f64 / 1024.0;
            let avail_kb = self.stack_available as f64 / 1024.0;
            let pct = used_kb / avail_kb * 100.0;
            log::info!(
                "program has used at least {:.2}/{:.2} KiB ({:.1}%) of stack space",
                used_kb,
                avail_kb,
                pct,
            );

            // Don't test for stack overflows if we're measuring stack usage.
            Ok(false)
        } else {
            match min_stack_usage {
                Some(min_stack_usage) => {
                    let used_kb = min_stack_usage as f64 / 1024.0;
                    let avail_kb = self.stack_available as f64 / 1024.0;
                    let pct = used_kb / avail_kb * 100.0;
                    log::warn!(
                        "program has used at least {:.2}/{:.2} KiB ({:.1}%) of stack space",
                        used_kb,
                        avail_kb,
                        pct,
                    );

                    if self.data_below_stack {
                        log::warn!("data segments might be corrupted due to stack overflow");
                    }

                    Ok(true)
                }
                None => {
                    log::debug!("stack canary intact");
                    Ok(false)
                }
            }
        }
    }
}

fn round_up(n: u32, k: u32) -> u32 {
    let rem = n % k;
    if rem == 0 {
        n
    } else {
        n + 4 - rem
    }
}

/// Write [`CANARY_VALUE`] to the stack.
///
/// Both `start` and `size` need to be 4-byte-aligned.
///
/// The [`Core`] is expected to be halted and will also be halted when this function returns.
fn paint_stack(core: &mut Core, start: u32, size: u32) -> Result<(), probe_rs::Error> {
    // does the subroutine fit inside the stack?
    assert!(
        PAINT_SUBROUTINE_LENGTH <= size as usize,
        "subroutine doesn't fit inside stack"
    );

    // write subroutine to RAM
    // NOTE: add `SUBROUTINE_LENGTH` to `start`, to avoid the subroutine overwriting itself
    let subroutine = paint_subroutine(start + PAINT_SUBROUTINE_LENGTH as u32, size);
    core.write_8(start, &subroutine)?;

    // store current PC and set PC to beginning of subroutine
    let previous_pc = core.read_core_reg(PC)?;
    core.write_core_reg(PC, start)?;

    // execute the subroutine and wait for it to finish
    core.run()?;
    core.wait_for_core_halted(TIMEOUT)?;

    // overwrite subroutine
    core.write_8(start, &[CANARY_VALUE; PAINT_SUBROUTINE_LENGTH])?;

    // reset PC to where it was before
    core.write_core_reg(PC, previous_pc)?;

    Ok(())
}

/// The length of the `paint_subroutine`.
const PAINT_SUBROUTINE_LENGTH: usize = 28;

/// Create a subroutine to paint [`CANARY_VALUE`] from `start` till `start + size`.
///
/// Both `start` and `size` need to be 4-byte-aligned.
//
// Roughly corresponds to following assembly:
//
// 00000108 <start>:
//  108:   4803        ldr r0, [pc, #12]   ; (118 <end+0x2>)
//  10a:   4904        ldr r1, [pc, #16]   ; (11c <end+0x6>)
//  10c:   4a04        ldr r2, [pc, #16]   ; (120 <end+0xa>)
//
// 0000010e <loop>:
//  10e:   4281        cmp r1, r0
//  110:   d001        beq.n   116 <end>
//  112:   c004        stmia   r0!, {r2}
//  114:   e7fb        b.n 10e <loop>
//
// 00000116 <end>:
//  116:   be00        bkpt    0x0000
//  118:   20000100    .word   0x20000100  ; start
//  11c:   20000200    .word   0x20000200  ; end
//  120:   aaaaaaaa    .word   0xaaaaaaaa  ; pattern
fn paint_subroutine(start: u32, size: u32) -> [u8; PAINT_SUBROUTINE_LENGTH] {
    assert_eq!(start % 4, 0, "`start` needs to be 4-byte-aligned");
    assert_eq!(size % 4, 0, "`end` needs to be 4-byte-aligned");

    let end = start + size;

    // convert start and end address to bytes
    let [s1, s2, s3, s4] = start.to_le_bytes();
    let [e1, e2, e3, e4] = end.to_le_bytes();

    const CV: u8 = CANARY_VALUE;
    [
        0x03, 0x48, // ldr r0, [pc, #12]
        0x04, 0x49, // ldr r1, [pc, #16]
        0x04, 0x4a, // ldr r2, [pc, #16]
        // <loop>
        0x81, 0x42, // cmp r1, r0
        0x01, 0xD0, // beq.n   116 <end>
        0x04, 0xC0, // stmia   r0!, {r2}
        0xFB, 0xE7, // b.n 10e <loop>
        // <end>
        0x00, 0xBE, // bkpt    0x0000
        //
        s1, s2, s3, s4, // .word ; start address
        e1, e2, e3, e4, // .word ; end address
        CV, CV, CV, CV, // .word ; canary value
    ]
}

/// Create a subroutine to measure if the stack grew into the painted area.
///
/// Returns the lowest address which does not contain the [CANARY_VALUE] anymore.
///
/// Both `start` and `size` need to be 4-byte-aligned.
///
/// The [`Core`] is expected to be halted and will also be halted when this function returns.
fn measure_stack(core: &mut Core, start: u32, size: u32) -> Result<Option<u32>, probe_rs::Error> {
    // does the subroutine fit inside the stack?
    assert!(
        MEASURE_SUBROUTINE_LENGTH <= size as usize,
        "subroutine doesn't fit inside stack"
    );

    let subroutine_address = start + size - MEASURE_SUBROUTINE_LENGTH as u32;

    // 1. use probe-rs to search through MEASURE_SUBROUTINE_LENGTH bytes
    let mut canary = [0; MEASURE_SUBROUTINE_LENGTH];
    core.read_8(subroutine_address, &mut canary)?;

    // 2a. If we found a touched byte, return the address of that
    if let Some(addr) = canary.iter().position(|b| *b != CANARY_VALUE) {
        return Ok(Some(addr as u32));
    }
    // 2b. If we did not, continue

    // 3. Place subroutine in the area we've searched through
    let subroutine = measure_subroutine(start, size - PAINT_SUBROUTINE_LENGTH as u32);
    core.write_8(subroutine_address, &subroutine)?;

    // 4. Execute subroutine
    let previous_pc = core.read_core_reg(PC)?;
    core.write_core_reg(PC, subroutine_address)?;
    core.run()?;
    core.wait_for_core_halted(TIMEOUT)?;
    core.write_core_reg(PC, previous_pc)?;

    const INITIAL_VALUE: u32 = 0;
    let touched_value_address = core.read_core_reg(CoreRegisterAddress(0))?; // TODO: how to read value from register?
    match touched_value_address == INITIAL_VALUE {
        // 5a. If any address got touched, return Some(address)
        false => Ok(Some(touched_value_address)),

        // 5b. If no address go touched, return None
        true => Ok(None),
    }
}

/// The length of the `measure_subroutine`.
const MEASURE_SUBROUTINE_LENGTH: usize = 0;

/// TODO
fn measure_subroutine(start: u32, size: u32) -> [u8; MEASURE_SUBROUTINE_LENGTH] {
    todo!()
}
