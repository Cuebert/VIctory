//! Bounded metadata correlation for the 64-byte laser AS header. No payload access.
use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

/// Kinds 1 and 2 identify a physical section or an owned registered buffer.
/// Kind 3 is an observed reservation and cannot establish live backing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Range {
    pub kind: u8,
    pub generation: u64,
    pub offset: u64,
    pub bytes: u64,
}

impl Range {
    fn overlap(self, rhs: Self) -> Option<(u64, u64)> {
        if self.kind != rhs.kind || self.generation != rhs.generation {
            return None;
        }
        let lo = self.offset.max(rhs.offset);
        let hi = self
            .offset
            .checked_add(self.bytes)?
            .min(rhs.offset.checked_add(rhs.bytes)?);
        (lo < hi).then_some((lo, hi.saturating_sub(lo)))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Probe {
    address: u64,
    bytes: u64,
    recorded: Option<Range>,
}

impl Probe {
    pub(crate) fn new(address: u64, bytes: u64) -> Self {
        Self {
            address,
            bytes,
            recorded: resolve(address, bytes),
        }
    }

    fn current(self) -> Option<Range> {
        self.recorded
            .filter(|r| Some(*r) == resolve(self.address, self.bytes))
    }
}

fn resolve(address: u64, bytes: u64) -> Option<Range> {
    if bytes == 0 || address == 0 {
        return None;
    }
    address.checked_add(bytes)?;
    let (kind, generation, offset) = crate::resource::as_range_identity(address, bytes)?;
    offset.checked_add(bytes)?;
    Some(Range {
        kind,
        generation,
        offset,
        bytes,
    })
}

#[derive(Clone, Copy)]
pub(crate) enum Event {
    Builder {
        target: Probe,
        shader: u64,
        slot: u8,
        issued: bool,
    },
    Header {
        target: Probe,
        issued: bool,
    },
}

// Per list and per selected frame. Replaying a closed list gets a new submission stamp.
pub(crate) const LIST_LIMIT: usize = 32;
const COPY_LIMIT: usize = 192;
const BUILDER_LIMIT: usize = 96;
const REPORT_LIMIT: usize = 16;

#[derive(Debug, Clone, Copy)]
pub(crate) struct At {
    pub fence: u64,
    pub list: usize,
    pub ordinal: usize,
}

#[derive(Debug, Clone, Copy)]
struct Builder {
    at: At,
    shader: u64,
    slot: u8,
    issued: bool,
    target: Range,
}
#[derive(Debug, Clone, Copy)]
struct CopyEvent {
    at: At,
    src: Option<Range>,
    dst: Range,
}

#[derive(Default)]
struct History {
    frame: Option<u64>,
    copies: VecDeque<CopyEvent>,
    builders: VecDeque<Builder>,
    attempted_copies: usize,
    reports: usize,
    truncated: bool,
}
static HISTORY: LazyLock<Mutex<History>> = LazyLock::new(|| Mutex::new(History::default()));

impl History {
    fn select(&mut self, frame: u64) {
        if self.frame != Some(frame) {
            if self.frame.is_none_or(|old| frame != old.saturating_add(1)) {
                *self = Self::default();
            }
            self.frame = Some(frame);
            self.reports = 0;
            self.attempted_copies = 0;
        }
    }
    fn builder(&mut self, builder: Builder) {
        if self.builders.len() == BUILDER_LIMIT {
            self.builders.pop_front();
            self.truncated = true;
        }
        self.builders.push_back(builder);
    }
    fn copied(&mut self, copy: CopyEvent) {
        if self.copies.len() == COPY_LIMIT {
            self.copies.pop_front();
            self.truncated = true;
        }
        self.copies.push_back(copy);
    }
    fn matching_builder(
        &self,
        range: Range,
        before_fence: Option<u64>,
        issued: bool,
    ) -> Option<(Builder, u64, u64)> {
        self.builders
            .iter()
            .rev()
            .filter(|b| b.issued == issued && before_fence.is_none_or(|f| b.at.fence < f))
            .find_map(|b| {
                let (offset, bytes) = b.target.overlap(range)?;
                Some((*b, offset, bytes))
            })
    }
    fn copy_chain(
        &self,
        header: Range,
    ) -> Option<(CopyEvent, u64, u64, Option<(Builder, u64, u64)>)> {
        self.copies.iter().rev().find_map(|c| {
            let (offset, bytes) = c.dst.overlap(header)?;
            let builder = c.src.and_then(|src| {
                let off = src.offset.checked_add(offset.checked_sub(c.dst.offset)?)?;
                let end = off.checked_add(bytes)?;
                if end > src.offset.checked_add(src.bytes)? {
                    return None;
                }
                // All CPU preparation in a batch happens before all GPU work in that batch.
                self.matching_builder(
                    Range {
                        offset: off,
                        bytes,
                        ..src
                    },
                    Some(c.at.fence),
                    true,
                )
            });
            Some((*c, offset, bytes, builder))
        })
    }
}

fn locked() -> std::sync::MutexGuard<'static, History> {
    HISTORY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run only for actual title-buffer copies in selected recording windows. Capped before looking up ranges.
pub(crate) fn prepare_copy(
    frame: u64,
    dst: usize,
    src: usize,
    bytes: usize,
) -> Option<(Probe, Probe)> {
    if !crate::capture::trace_window(frame) {
        return None;
    }
    let mut h = locked();
    h.select(frame);
    if h.attempted_copies >= COPY_LIMIT {
        h.truncated = true;
        return None;
    }
    h.attempted_copies += 1;
    drop(h);
    let dst = Probe::new(dst as u64, bytes as u64);
    dst.recorded?;
    Some((dst, Probe::new(src as u64, bytes as u64)))
}

pub(crate) fn finish_copy(at: At, (dst, src): (Probe, Probe)) {
    if let Some(dst) = dst.current() {
        let src = src.current();
        locked().copied(CopyEvent { at, src, dst });
    }
}

pub(crate) fn native(frame: u64, at: At, event: Event, list_truncated: bool) {
    // Resolve outside the history lock. A changed registry generation never aliases an earlier event.
    let range = match event {
        Event::Builder { target, .. } | Event::Header { target, .. } => target.current(),
    };
    let mut h = locked();
    h.select(frame);
    h.truncated |= list_truncated;
    match event {
        Event::Builder {
            shader,
            slot,
            issued,
            ..
        } => {
            if let Some(target) = range {
                h.builder(Builder {
                    at,
                    shader,
                    slot,
                    issued,
                    target,
                });
            }
        }
        Event::Header { issued, .. }
            if h.reports < REPORT_LIMIT && crate::capture::listed(frame) =>
        {
            h.reports += 1;
            let builder = range.and_then(|r| h.matching_builder(r, None, true));
            let rejected = range.and_then(|r| h.matching_builder(r, None, false));
            let copied = range.and_then(|r| h.copy_chain(r));
            let truncated = h.truncated;
            let producer = builder.as_ref().map(|(b, _, _)| (b.shader, b.slot));
            drop(h);
            // All offsets are relative to the generation named by the corresponding range.
            // This is event overlap, not proof that a shader wrote every bound byte.
            tracing::info!(frame, queue="host-direct", fence=at.fence, list=at.list, event=at.ordinal,
                issued, header=?range, ?producer, ?builder, ?rejected, cpu_copy=?copied, truncated,
                "AS header causal observations (kind 3 is historical only)");
        }
        _ => {}
    }
}

/// Published only after an exact write through the originally retained section.
/// Reports in earlier commands of the same batch precede this publication.
pub(crate) fn published_reset(frame:u64,at:At,target:Range) {
    let mut history=locked();
    history.select(frame);
    history.builder(Builder{at,shader:0x3b90165a77d95cdf,slot:1,issued:true,target});
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(id: u64, offset: u64, bytes: u64) -> Range {
        Range {
            kind: 1,
            generation: id,
            offset,
            bytes,
        }
    }
    fn at(fence: u64) -> At {
        At {
            fence,
            list: 0,
            ordinal: 0,
        }
    }
    fn builder(target: Range, fence: u64, issued: bool) -> Builder {
        Builder {
            at: at(fence),
            shader: 1,
            slot: 2,
            issued,
            target,
        }
    }

    #[test]
    fn alias_uses_backing_and_offset_and_generation() {
        let mut h = History::default();
        h.builder(builder(r(11, 80, 128), 5, true));
        assert_eq!(
            h.matching_builder(r(11, 112, 64), None, true).unwrap().2,
            64
        );
        assert!(h.matching_builder(r(12, 112, 64), None, true).is_none());
        assert!(h.matching_builder(r(11, 208, 64), None, true).is_none());
        assert_eq!(h.matching_builder(r(11, 200, 64), None, true).unwrap().2, 8);
        assert!(
            h.matching_builder(
                Range {
                    kind: 3,
                    ..r(11, 112, 64)
                },
                None,
                true
            )
            .is_none()
        );
    }
    #[test]
    fn copies_translate_offsets_and_respect_actual_phase() {
        let mut h = History::default();
        h.builder(builder(r(2, 112, 64), 4, true));
        h.builder(builder(r(2, 112, 64), 5, true));
        h.copied(CopyEvent {
            at: at(5),
            src: Some(r(2, 100, 128)),
            dst: r(7, 500, 128),
        });
        let (_, offset, n, b) = h.copy_chain(r(7, 512, 64)).unwrap();
        assert_eq!((offset, n, b.unwrap().0.at.fence), (512, 64, 4));
        assert!(h.copy_chain(r(8, 512, 64)).is_none());
        h.builders.clear();
        h.builder(builder(r(2, 112, 64), 5, true));
        h.builder(builder(r(2, 112, 64), 3, false));
        assert!(h.copy_chain(r(7, 512, 64)).unwrap().3.is_none());
    }
    #[test]
    fn bounded_history_drops_old_rows_and_marks_incomplete() {
        let mut h = History::default();
        h.select(3);
        for n in 0..BUILDER_LIMIT + 4 {
            h.builder(builder(r(n as u64, 0, 64), 1, true));
        }
        assert_eq!(h.builders.len(), BUILDER_LIMIT);
        assert!(h.truncated);
        h.select(8);
        assert!(h.builders.is_empty());
        assert!(!h.truncated);
    }
}
