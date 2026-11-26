//! Which lanes of a wave set a write or read mark (module notes of `emit`,
//! "Write marks", "Read marks").

use super::super::bitcode::{Attr, Bin, V, pred};
use super::super::shader::flags;
use super::Lower;

impl Lower<'_> {
    /// Whether this lane sets mark bit `n` (its load and atomic go to the
    /// bitmap in system memory): lanes with the first active lane's bit
    /// leave it to that lane. Most lanes of a wave mark the same page, so
    /// one load crosses the bus instead of 32. In pixel shaders the lane
    /// that leads is the first that is no helper: a helper's writes are
    /// dropped.
    pub(super) fn mark_lead(&mut self, n: V) -> Option<V> {
        if !self.wave_marks {
            return None;
        }
        self.s.flags |= flags::WAVE_OPS;
        let c = self.cur;
        let (i1, i32t) = (self.s.t.i1, self.s.t.i32);
        let (f, first) = if self.s.pixel {
            let helper = self
                .s
                .op(c, "isHelperLane.i1", Attr::ReadOnly, i1, 221, &[]);
            let lane = self
                .s
                .op(c, "waveGetLaneIndex", Attr::ReadNone, i32t, 111, &[]);
            // A helper's key is its lane with a high bit: the minimum is the
            // first non-helper, or (all helpers) a helper no lane matches,
            // still a lane to read at (writes there are dropped anyway).
            let flag = self.s.m.i32(1 << 30);
            let flagged = self.s.m.bin(c, Bin::Or, lane, flag);
            let key = self.s.m.select(c, helper, flagged, lane);
            // Operation 2, unsigned: the minimum.
            let (min, unsigned) = (self.s.m.i8(2), self.s.m.i8(1));
            let lead = self.s.op(
                c,
                "waveActiveOp.i32",
                Attr::NoUnwind,
                i32t,
                119,
                &[key, min, unsigned],
            );
            let low = self.s.m.i32((1 << 30) - 1);
            let at = self.s.m.bin(c, Bin::And, lead, low);
            let f = self
                .s
                .op(c, "waveReadLaneAt.i32", Attr::NoUnwind, i32t, 117, &[n, at]);
            (f, self.s.m.cmp(c, pred::EQ, lane, lead))
        } else {
            let f = self
                .s
                .op(c, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[n]);
            (
                f,
                self.s
                    .op(c, "waveIsFirstLane", Attr::NoUnwind, i1, 110, &[]),
            )
        };
        let differ = self.s.m.cmp(c, pred::NE, n, f);
        Some(self.s.m.bin(c, Bin::Or, differ, first))
    }

    /// Marks the pages a write of `bytes` bytes at byte `x` of the unordered
    /// buffer view `h` touches, in the lanes `guard` lets through (module
    /// notes, "Write marks"): the bits of its first and last byte, each set
    /// only where still clear. Nothing for handles that are no heap view, or
    /// without [`super::Parts::view_marks`].
    pub(super) fn mark_view_write(&mut self, h: V, x: V, bytes: u32, guard: Option<V>) {
        if !self.x.view_marks {
            return;
        }
        let Some(&index) = self.s.view_index.get(&h) else {
            return;
        };
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        let table = self.s.view_marks_handle();
        let c = self.cur;
        let eight = k(self, 8);
        let at = self.s.m.bin(c, Bin::Mul, index, eight);
        let (base, phase) = self.raw_dword_pair(table, at);
        let none = k(self, super::super::VIEW_MARK_NONE);
        let valid = self.s.m.cmp(c, pred::NE, base, none);
        let active = match guard {
            Some(g) => self.s.m.bin(c, Bin::And, g, valid),
            None => valid,
        };
        let first = self.s.m.bin(c, Bin::Add, phase, x);
        let span = k(self, bytes.max(1) - 1);
        let last = self.s.m.bin(c, Bin::Add, first, span);
        let (k16, cap) = (k(self, 16), k(self, super::super::VIEW_WRITE_BITS - 1));
        let mut bits = Vec::new();
        for byte in [first, last] {
            let page = self.s.m.bin(c, Bin::LShr, byte, k16);
            let n = self.s.m.bin(c, Bin::Add, base, page);
            let over = self.s.m.cmp(c, pred::IUGT, n, cap);
            bits.push(self.s.m.select(c, over, cap, n));
        }
        let bitmap = self.s.view_writes_handle();
        let outer = self.begin_guarded(Some(active));
        for (i, &n) in bits.iter().enumerate() {
            let c = self.cur;
            let (k5, k2, k31, one, zero) = (k(self, 5), k(self, 2), k(self, 31), k(self, 1), k(self, 0));
            let word = self.s.m.bin(c, Bin::LShr, n, k5);
            let word = self.s.m.bin(c, Bin::Shl, word, k2);
            let bit = self.s.m.bin(c, Bin::And, n, k31);
            let bit = self.s.m.bin(c, Bin::Shl, one, bit);
            let leader = self.mark_lead(n);
            let led = self.begin_guarded(leader);
            let c = self.cur;
            // Most writes find their page marked: a load, not an atomic.
            let old = self.raw_dword(bitmap, word);
            let set = self.s.m.bin(c, Bin::And, old, bit);
            let mut clear = self.s.m.cmp(c, pred::EQ, set, zero);
            if i == 1 {
                let other = self.s.m.cmp(c, pred::NE, n, bits[0]);
                clear = self.s.m.bin(c, Bin::And, clear, other);
            }
            let inner = self.begin_guarded(Some(clear));
            let iu = self.s.m.undef(self.s.t.i32);
            // Operation 2 is OR.
            self.s.op(self.cur, "atomicBinOp.i32", Attr::NoUnwind, self.s.t.i32, 78, &[bitmap, k2, word, iu, iu, bit]);
            self.end_guarded(inner, &[]);
            self.end_guarded(led, &[]);
        }
        self.end_guarded(outer, &[]);
    }

    /// Sets bit `n` in the title-memory dirty bitmap `bitmap`.
    /// Called inside the access guard: only lanes that owe a mark participate.
    pub(super) fn mark_memory_bit(&mut self, bitmap: V, n: V) {
        let c = self.cur;
        let [five, two, low, one] = [5, 2, 31, 1].map(|k| self.s.m.i32(k));
        let word = self.s.m.bin(c, Bin::LShr, n, five);
        let offset = self.s.m.bin(c, Bin::Shl, word, two);
        let shift = self.s.m.bin(c, Bin::And, n, low);
        let bit = self.s.m.bin(c, Bin::Shl, one, shift);

        // With helper lanes included, the first lane in a pixel wave could
        // be a helper whose stores are dropped. Keep those marks per lane.
        let (writer, mask) = if self.wave_marks && !self.s.pixel {
            // One active lane can mark the whole wave only when the word is shared.
            // Otherwise each lane keeps its own word and bit. No fixed wave size.
            self.s.flags |= flags::WAVE_OPS;
            let same = self.s.op(
                c,
                "waveActiveAllEqual.i32",
                Attr::NoUnwind,
                self.s.t.i1,
                115,
                &[word],
            );
            let or = self.s.m.i8(1);
            let bits = self.s.op(
                c,
                "waveActiveBit.i32",
                Attr::NoUnwind,
                self.s.t.i32,
                120,
                &[bit, or],
            );
            let first = self
                .s
                .op(c, "waveIsFirstLane", Attr::NoUnwind, self.s.t.i1, 110, &[]);
            let mask = self.s.m.select(c, same, bits, bit);
            let yes = self.s.m.bool(true);
            let writer = self.s.m.select(c, same, first, yes);
            (Some(writer), mask)
        } else {
            (None, bit)
        };
        let at = self.begin_guarded(writer);

        // Until the submission finishes bits only accumulate. A stale load can
        // cause an extra OR, and a partially set mask still needs the atomic.
        let old = self.raw_dword(bitmap, offset);
        let set = self.s.m.bin(self.cur, Bin::And, old, mask);
        let missing = self.s.m.cmp(self.cur, pred::NE, set, mask);
        let update = self.begin_guarded(Some(missing));
        let iu = self.s.m.undef(self.s.t.i32);
        self.s.op(
            self.cur,
            "atomicBinOp.i32",
            Attr::NoUnwind,
            self.s.t.i32,
            78,
            &[bitmap, two, offset, iu, iu, mask],
        );
        self.end_guarded(update, &[]);
        self.end_guarded(at, &[]);
    }

    /// Sets the read mark of resource-heap index `index` (module notes,
    /// "Read marks") where it is still clear. Nothing without
    /// [`super::Parts::view_marks`].
    pub(super) fn mark_view_read(&mut self, index: V) {
        if self.x.view_marks {
            self.mark_read_bit(super::super::VIEW_READS, index, true);
        }
    }

    /// Sets bit `base` + `n` of the view-writes bitmap: where still clear
    /// (`tested`), else with an atomic alone (inside guarded code, which
    /// cannot branch on).
    pub(super) fn mark_read_bit(&mut self, base: u32, n: V, tested: bool) {
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        let bitmap = self.s.view_writes_handle();
        let c = self.cur;
        let (base, k5, k2, k31, one, zero) =
            (k(self, base), k(self, 5), k(self, 2), k(self, 31), k(self, 1), k(self, 0));
        let n = self.s.m.bin(c, Bin::Add, n, base);
        let word = self.s.m.bin(c, Bin::LShr, n, k5);
        let word = self.s.m.bin(c, Bin::Shl, word, k2);
        let bit = self.s.m.bin(c, Bin::And, n, k31);
        let bit = self.s.m.bin(c, Bin::Shl, one, bit);
        // Handles made inside guarded code need the straight-line page mark.
        let leader = if tested { self.mark_lead(n) } else { None };
        let led = self.begin_guarded(leader);
        let c = self.cur;
        // Most reads find their bit set: a load, not an atomic.
        let at = if tested {
            let old = self.raw_dword(bitmap, word);
            let set = self.s.m.bin(c, Bin::And, old, bit);
            let clear = self.s.m.cmp(c, pred::EQ, set, zero);
            self.begin_guarded(Some(clear))
        } else {
            None
        };
        let iu = self.s.m.undef(self.s.t.i32);
        // Operation 2 is OR.
        self.s.op(self.cur, "atomicBinOp.i32", Attr::NoUnwind, self.s.t.i32, 78, &[bitmap, k2, word, iu, iu, bit]);
        self.end_guarded(at, &[]);
        self.end_guarded(led, &[]);
    }
}

#[cfg(all(test, windows))]
mod tests;
