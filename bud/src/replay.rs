//! The receiver's replay window: which of the last [`WINDOW`] packet numbers
//! below the highest one seen have already arrived. A datagram transport
//! reorders and duplicates; an attacker who records packets can replay them.
//! A packet number is accepted once, and only if it is neither older than the
//! window nor already seen.
//!
//! `check` and `accept` are separate on purpose: a packet is looked up before
//! it is decrypted, but only recorded after it authenticated, so forged packets
//! cannot move the window.

/// How many packet numbers back from the highest are remembered.
pub const WINDOW: u64 = 1024;
const WORDS: usize = (WINDOW / 64) as usize;

#[derive(Debug, Clone)]
pub struct ReplayWindow {
    highest: Option<u64>,
    /// Bit `k` (word `k / 64`, bit `k % 64`) says `highest - k` was seen.
    bits: [u64; WORDS],
}

impl Default for ReplayWindow {
    fn default() -> Self {
        ReplayWindow::new()
    }
}

impl ReplayWindow {
    pub fn new() -> Self {
        ReplayWindow {
            highest: None,
            bits: [0; WORDS],
        }
    }

    fn bit(&self, k: u64) -> bool {
        self.bits[(k / 64) as usize] >> (k % 64) & 1 == 1
    }

    fn set(&mut self, k: u64) {
        self.bits[(k / 64) as usize] |= 1 << (k % 64);
    }

    /// Whether `packet` could still be accepted: newer than everything seen,
    /// or inside the window and not seen yet. Does not record it.
    pub fn check(&self, packet: u64) -> bool {
        match self.highest {
            None => true,
            Some(highest) if packet > highest => true,
            Some(highest) => {
                let back = highest - packet;
                back < WINDOW && !self.bit(back)
            }
        }
    }

    /// Records `packet` as seen. Call it only for a packet that authenticated.
    pub fn accept(&mut self, packet: u64) {
        match self.highest {
            None => {
                self.highest = Some(packet);
                self.set(0);
            }
            Some(highest) if packet > highest => {
                self.shift(packet - highest);
                self.highest = Some(packet);
                self.set(0);
            }
            Some(highest) => {
                let back = highest - packet;
                if back < WINDOW {
                    self.set(back);
                }
            }
        }
    }

    /// Moves every remembered packet `by` places further back.
    fn shift(&mut self, by: u64) {
        if by >= WINDOW {
            self.bits = [0; WORDS];
            return;
        }
        let words = (by / 64) as usize;
        let bits = (by % 64) as u32;
        for i in (0..WORDS).rev() {
            let mut value = if i >= words { self.bits[i - words] } else { 0 };
            if bits > 0 {
                value <<= bits;
                if i > words {
                    value |= self.bits[i - words - 1] >> (64 - bits);
                }
            }
            self.bits[i] = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(window: &mut ReplayWindow, packet: u64) -> bool {
        let ok = window.check(packet);
        if ok {
            window.accept(packet);
        }
        ok
    }

    #[test]
    fn a_packet_is_accepted_once() {
        let mut w = ReplayWindow::new();
        assert!(take(&mut w, 0));
        assert!(!take(&mut w, 0));
        assert!(take(&mut w, 1));
        assert!(!take(&mut w, 1));
    }

    #[test]
    fn reordering_inside_the_window_is_fine_but_only_once() {
        let mut w = ReplayWindow::new();
        assert!(take(&mut w, 10));
        assert!(take(&mut w, 7));
        assert!(!take(&mut w, 7));
        assert!(take(&mut w, 9));
        assert!(!take(&mut w, 10));
    }

    #[test]
    fn a_packet_older_than_the_window_is_refused() {
        let mut w = ReplayWindow::new();
        assert!(take(&mut w, 5000));
        assert!(!take(&mut w, 5000 - WINDOW));
        // The oldest one still inside the window is accepted.
        assert!(take(&mut w, 5000 - WINDOW + 1));
    }

    #[test]
    fn a_big_jump_forgets_the_old_window_and_nothing_else() {
        let mut w = ReplayWindow::new();
        assert!(take(&mut w, 3));
        assert!(take(&mut w, 3 + 10 * WINDOW));
        assert!(!take(&mut w, 3));
        assert!(!take(&mut w, 3 + 10 * WINDOW));
        assert!(take(&mut w, 3 + 10 * WINDOW - 1));
    }

    #[test]
    fn shifting_across_word_boundaries_keeps_every_remembered_packet() {
        // Seen packets must stay seen for every shift distance, including
        // the ones that move whole words (64, 128) and ones that do not.
        for gap in [1u64, 2, 31, 63, 64, 65, 127, 128, 129, 200, 1000, 1023] {
            let mut w = ReplayWindow::new();
            let base = 5000;
            for p in [base, base - 1, base - 63, base - 64, base - 100] {
                assert!(take(&mut w, p));
            }
            assert!(take(&mut w, base + gap));
            for p in [base, base - 1, base - 63, base - 64, base - 100] {
                let back = base + gap - p;
                if back < WINDOW {
                    assert!(!w.check(p), "gap {gap}: {p} must still count as seen");
                }
            }
            // Something never seen, still in the window, is still acceptable.
            assert!(w.check(base + gap - 5));
        }
    }

    #[test]
    fn check_does_not_record() {
        let mut w = ReplayWindow::new();
        assert!(w.check(42));
        assert!(w.check(42));
        w.accept(42);
        assert!(!w.check(42));
    }
}
