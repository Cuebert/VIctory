//! The original alpha-transmission mip prefilter on a Wave32 host.
//! Its LDS producers and consumers meet at the original group barrier.

use crate::ShaderError;
use crate::dxbc::{Container, FourCC};
use crate::gfx10::Fields;
use crate::ir::{self, Program};
use crate::native::{NativeProgram, StageIo};
use crate::structure::{self, LaneExit, Structured};

const EMPTY_EXEC_EXIT: usize = 0xec;
const GROUP_BARRIER: usize = 0x9b0;

pub(super) fn selected(container: &Container, native: &NativeProgram) -> bool {
    native.wave64
        && matches!(native.io, StageIo::Cs(cs) if cs.threads == [256, 1, 1])
        && super::lower::host_max_wave() == 32
        && u128::from_be_bytes(container.digest) == 0xb5b83854_8eaa4050_638308fb_2c805641
        && container.part(FourCC::DXIL).is_some_and(|part| part.data.len() == 3808)
        && std::env::var_os("VICTORY_ALPHA_TRANSMISSION_WAVE32").is_some_and(|value| value == "1")
}

/// Keep the empty guest waves physically present. The original EXEC guards
/// still control their stores, including those in the producer region.
pub(super) fn keep_empty_waves(p: &Program, st: &Structured) -> Result<Structured, ShaderError> {
    let continuation = p.blocks.iter().position(|block| block.insts.last().is_some_and(|inst| {
        inst.machine.offset == EMPTY_EXEC_EXIT && inst.name() == "s_cbranch_execz"
    })).and_then(|branch| match st.exits[branch] {
        LaneExit::Branch { taken, not_taken, .. }
            if p.blocks[taken].exit == ir::Exit::End
                && p.blocks[taken].insts.iter().all(|inst| inst.name() == "s_endpgm") =>
        {
            Some((branch, not_taken))
        }
        _ => None,
    });
    let (branch, not_taken) = continuation.ok_or(ShaderError::Unsupported {
        offset: EMPTY_EXEC_EXIT,
        what: "the alpha prefilter's empty EXEC exit could not be joined",
    })?;
    let mut lowered = structure::structure_wide(p, &std::collections::HashSet::from([branch]))?;
    lowered.wide.remove(&branch);
    lowered.exits[branch] = LaneExit::Jump(not_taken);
    Ok(lowered)
}

pub(super) fn lds_handoff(p: &Program, st: &Structured) -> Result<Vec<bool>, ShaderError> {
    if super::group_sync() != 0 && let Some(common) = common_blocks(p, st) {
        return Ok(common);
    }
    Err(ShaderError::Unsupported {
        offset: GROUP_BARRIER,
        what: "the alpha prefilter has no converged LDS producer/consumer barrier",
    })
}

/// Require one reachable group barrier in an acyclic graph. Shared-memory
/// writes must precede it, and reads must follow it. An injected exchange
/// may run only in a block reached on every path to an exit.
fn common_blocks(p: &Program, st: &Structured) -> Option<Vec<bool>> {
    let successors = |block: usize| match st.exits[block] {
        LaneExit::Jump(target) => [Some(target), None],
        LaneExit::Branch { taken, not_taken, .. } => [Some(taken), Some(not_taken)],
        LaneExit::End | LaneExit::Indirect => [None, None],
    };
    let visit = |skip: Option<usize>| {
        let mut seen = vec![false; p.blocks.len()];
        let mut pending = vec![0];
        while let Some(block) = pending.pop() {
            if Some(block) == skip || seen[block] {
                continue;
            }
            seen[block] = true;
            pending.extend(successors(block).into_iter().flatten());
        }
        seen
    };
    let reachable = visit(None);
    let mut incoming = vec![0usize; p.blocks.len()];
    let mut barrier = None;
    for (block, &seen) in reachable.iter().enumerate() {
        if !seen {
            continue;
        }
        if matches!(st.exits[block], LaneExit::Indirect) {
            return None;
        }
        for target in successors(block).into_iter().flatten() {
            incoming[target] += 1;
        }
        for (index, inst) in p.blocks[block].insts.iter().enumerate() {
            if inst.name() == "s_barrier"
                && (inst.machine.offset != GROUP_BARRIER || barrier.replace((block, index)).is_some())
            {
                return None;
            }
        }
    }
    let (meeting, position) = barrier?;
    if incoming[0] != 0 {
        return None;
    }
    let mut pending = vec![0];
    let mut processed = 0;
    while let Some(block) = pending.pop() {
        processed += 1;
        for target in successors(block).into_iter().flatten() {
            incoming[target] -= 1;
            if incoming[target] == 0 {
                pending.push(target);
            }
        }
    }
    if processed != reachable.iter().filter(|&&seen| seen).count() {
        return None;
    }
    let exits: Vec<_> = reachable.iter().enumerate().filter_map(|(block, &seen)| {
        (seen && matches!(st.exits[block], LaneExit::End)).then_some(block)
    }).collect();
    let before = visit(Some(meeting));
    if exits.iter().any(|&block| before[block]) {
        return None;
    }
    let (mut writes, mut reads) = (false, false);
    for (block, body) in p.blocks.iter().enumerate().filter(|(block, _)| reachable[*block]) {
        for (index, inst) in body.insts.iter().enumerate() {
            let name = inst.name();
            if !matches!(inst.fields, Fields::Ds { .. }) || name.starts_with("ds_swizzle") || name.contains("permute") {
                continue;
            }
            let producer = before[block] || (block == meeting && index < position);
            if producer && name.starts_with("ds_write") {
                writes = true;
            } else if !producer && name.starts_with("ds_read") {
                reads = true;
            } else {
                return None;
            }
        }
    }
    (writes && reads).then(|| reachable.iter().enumerate().map(|(block, &seen)| {
        seen && {
            let avoiding = visit(Some(block));
            exits.iter().all(|&end| !avoiding[end])
        }
    }).collect())
}
