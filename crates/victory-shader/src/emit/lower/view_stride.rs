//! Per-view metadata for buffer operations.

use super::{Bin, Lower, V, VIEW_INTEGER, VIEW_RAW_OFFSET_SHIFT, VIEW_SIGNED, VIEW_STRIDE_BITS, VIEW_STRIDES, pred};

impl Lower<'_> {
    /// The byte stride of the buffer view at heap index `index`, from the
    /// view-stride table (`t`[`VIEW_STRIDES`]).
    pub(super) fn stride_at(&mut self, index: V) -> V {
        let word = self.view_word(index);
        let bits = self.s.m.i32(VIEW_STRIDE_BITS);
        self.s.m.bin(self.cur, Bin::And, word, bits)
    }

    /// Dword `index` of the view-stride table: the stride and the format
    /// kind bits ([`VIEW_INTEGER`], [`VIEW_SIGNED`]).
    pub(super) fn view_word(&mut self, index: V) -> V {
        let c = self.cur;
        let four = self.s.m.i32(4);
        let at = self.s.m.bin(c, Bin::Mul, index, four);
        let table = self.s.root_srv_handle(VIEW_STRIDES);
        self.raw_dword(table, at)
    }

    /// Whether the typed buffer view a handle names has an integer format,
    /// and whether a signed one, from the view-stride table; `None` if the
    /// handle's heap index is not known.
    pub(super) fn integer_view(&mut self, h: V) -> Option<(V, V)> {
        let index = *self.s.view_index.get(&h)?;
        let word = self.view_word(index);
        let c = self.cur;
        let zero = self.s.m.i32(0);
        let [int, signed] = [VIEW_INTEGER, VIEW_SIGNED].map(|bit| {
            let bit = self.s.m.i32(bit);
            let x = self.s.m.bin(c, Bin::And, word, bit);
            self.s.m.cmp(c, pred::NE, x, zero)
        });
        Some((int, signed))
    }

    /// Byte offset (`0`, `4`, `8`, or `12`) from a raw view's 16-byte-aligned
    /// host descriptor start to the title view's actual start, stored in
    /// [`VIEW_STRIDES`] bits 28..29; `None` if the handle's heap index is not
    /// known.
    pub(super) fn view_raw_bias(&mut self, h: V) -> Option<V> {
        if let Some(&(block, cached)) = self.table_bias.get(&h) {
            if let Some(bias) = cached {
                return Some(bias);
            }
            // An entry or join handle may be used in separate branches. Its
            // metadata belongs beside the handle so it dominates every use.
            let index = self.s.view_index[&h];
            let here = std::mem::replace(&mut self.cur, block);
            let word = self.view_word(index);
            let bias = self.decode_raw_bias(word);
            self.cur = here;
            self.table_bias.insert(h, (block, Some(bias)));
            return Some(bias);
        }
        let word = match self.guarded_stride.get(&h).copied() {
            Some(word) => word,
            None => {
                let index = *self.s.view_index.get(&h)?;
                self.view_word(index)
            }
        };
        Some(self.decode_raw_bias(word))
    }

    fn decode_raw_bias(&mut self, word: V) -> V {
        let c = self.cur;
        let shift = self.s.m.i32(VIEW_RAW_OFFSET_SHIFT - 2);
        let mask = self.s.m.i32(0xC);
        let shifted = self.s.m.bin(c, Bin::LShr, word, shift);
        self.s.m.bin(c, Bin::And, shifted, mask)
    }
}
