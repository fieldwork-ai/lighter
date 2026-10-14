//! vsock flow control.
//!
//! vsock has no windowing in the TCP sense. Each side publishes two numbers —
//! how big its receive buffer is, and how many bytes its application has
//! consumed in total — and the sender works out for itself how much it may
//! send. Get it wrong in one direction and the peer's buffer overruns; get it
//! wrong in the other and the connection stalls forever with both sides
//! waiting.
//!
//! Every counter here is `u32` and wraps, which is not a rounding error to be
//! tidied away: the protocol defines them as wrapping, and the arithmetic below
//! is written in wrapping operations for that reason. A connection that has
//! moved four gigabytes is an ordinary thing, and it must not be the moment
//! everything stops.

/// The most we tell the peer our receive buffer is: a connection's full
/// window. Large enough that a stream is not a sequence of stop-and-wait
/// round trips at tens of gigabits.
pub const BUF_ALLOC: u32 = 4 * 1024 * 1024;

/// The least: what a connection keeps however tight the budget, so it can
/// always make progress. A quarter of it is the credit-update threshold.
pub const MIN_WINDOW: u32 = 64 * 1024;

/// What every window together may promise. A window is what the guest may
/// send us ahead of the host peer taking it, and a payload beyond a
/// connection's share of the transmit ring is copied out to be held here,
/// so the sum of the windows is what bounds the host's memory: 32 streams at
/// a full window, or two thousand at the least.
pub const BUDGET: u64 = 128 * 1024 * 1024;

/// The windows every connection advertises, drawn from one budget.
///
/// A window is granted in full while the budget allows, grows back by
/// doubling as its connection drains, and is cut to the least when its host
/// peer stops reading or it sits idle, so that windows follow traffic and a
/// stalled peer cannot keep the guest sending into memory nobody will
/// empty. The guest's own kernel bounds what it has in flight by the
/// latest window it heard, shrinking or not (6.18's `has_space` is signed).
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    total: u64,
    granted: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Budget::new(BUDGET)
    }
}

impl Budget {
    pub const fn new(total: u64) -> Budget {
        Budget { total, granted: 0 }
    }

    fn free(&self) -> u64 {
        self.total.saturating_sub(self.granted)
    }

    /// A new connection's window: as much of a full one as is free, and
    /// never less than the least.
    pub fn open(&mut self) -> u32 {
        let window = self
            .free()
            .min(u64::from(BUF_ALLOC))
            .max(u64::from(MIN_WINDOW)) as u32;
        self.granted += u64::from(window);
        window
    }

    /// The window after a connection has drained a quarter of `window`:
    /// doubled, as far as the budget and a full window allow.
    pub fn grow(&mut self, window: u32) -> u32 {
        let want = (u64::from(window) * 2).min(u64::from(BUF_ALLOC));
        let grown = want
            .min(u64::from(window) + self.free())
            .max(u64::from(window));
        self.granted += grown - u64::from(window);
        grown as u32
    }

    /// The least window, for a connection whose peer has stopped or that
    /// has gone idle.
    pub fn shrink(&mut self, window: u32) -> u32 {
        let least = window.min(MIN_WINDOW);
        self.granted = self.granted.saturating_sub(u64::from(window - least));
        least
    }

    /// A connection is gone and its window with it.
    pub fn close(&mut self, window: u32) {
        self.granted = self.granted.saturating_sub(u64::from(window));
    }

    /// What every window together promises now.
    pub const fn granted(&self) -> u64 {
        self.granted
    }
}

/// One direction's accounting for one connection.
#[derive(Debug, Clone, Copy)]
pub struct Credit {
    /// The size of the peer's receive buffer, as the peer last reported it.
    peer_buf_alloc: u32,
    /// How many bytes the peer's application has consumed, as last reported.
    peer_fwd_cnt: u32,
    /// How many bytes we have sent to the peer.
    tx_cnt: u32,
    /// How many bytes we have handed to our own application.
    fwd_cnt: u32,
}

impl Default for Credit {
    fn default() -> Self {
        Credit::new()
    }
}

impl Credit {
    pub const fn new() -> Credit {
        Credit {
            // Zero until the peer tells us otherwise, which means we may send
            // nothing before the handshake completes. That is the correct
            // starting point: assuming a buffer the peer has not claimed is how
            // you overrun it on the very first write.
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            tx_cnt: 0,
            fwd_cnt: 0,
        }
    }

    /// Records the credit fields carried by every packet from the peer.
    /// The receive buffer the peer advertised.
    pub const fn peer_buf_alloc(&self) -> u32 {
        self.peer_buf_alloc
    }

    /// The counters, for a trace: (peer_buf_alloc, peer_fwd_cnt, tx_cnt, fwd_cnt).
    pub fn counters(&self) -> (u32, u32, u32, u32) {
        (
            self.peer_buf_alloc,
            self.peer_fwd_cnt,
            self.tx_cnt,
            self.fwd_cnt,
        )
    }

    pub fn observe(&mut self, buf_alloc: u32, fwd_cnt: u32) {
        self.peer_buf_alloc = buf_alloc;
        self.peer_fwd_cnt = fwd_cnt;
    }

    /// How many bytes we may send right now.
    pub fn available(&self) -> u32 {
        // Bytes in flight: sent by us, not yet consumed by the peer's
        // application. Wrapping, because both counters wrap independently.
        let in_flight = self.tx_cnt.wrapping_sub(self.peer_fwd_cnt);
        // A peer that has consumed more than we sent would make this negative;
        // saturating to zero means we stall rather than send into a buffer we
        // cannot reason about.
        self.peer_buf_alloc.saturating_sub(in_flight)
    }

    /// Records bytes handed to the peer.
    pub fn sent(&mut self, bytes: u32) {
        self.tx_cnt = self.tx_cnt.wrapping_add(bytes);
    }

    /// Records bytes our application has consumed, which is what frees the
    /// peer to send more.
    pub fn consumed(&mut self, bytes: u32) {
        self.fwd_cnt = self.fwd_cnt.wrapping_add(bytes);
    }

    /// Our own buffer size, for the header.
    pub const fn buf_alloc(&self) -> u32 {
        BUF_ALLOC
    }

    /// Our consumed count, for the header.
    pub const fn fwd_cnt(&self) -> u32 {
        self.fwd_cnt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_nothing_before_the_peer_has_advertised_anything() {
        // The handshake has not happened. Sending here overruns a buffer whose
        // size we are guessing.
        assert_eq!(Credit::new().available(), 0);
    }

    #[test]
    fn available_shrinks_as_we_send_and_recovers_as_the_peer_consumes() {
        let mut credit = Credit::new();
        credit.observe(1000, 0);
        assert_eq!(credit.available(), 1000);

        credit.sent(400);
        assert_eq!(credit.available(), 600, "400 bytes are in flight");

        credit.observe(1000, 400);
        assert_eq!(credit.available(), 1000, "the peer consumed all of them");
    }

    #[test]
    fn a_full_peer_buffer_stalls_rather_than_overruns() {
        let mut credit = Credit::new();
        credit.observe(1000, 0);
        credit.sent(1000);
        assert_eq!(credit.available(), 0);
    }

    /// The counters are defined as wrapping, and a connection that has moved
    /// four gigabytes is ordinary. If this arithmetic panicked in debug or
    /// went negative in release, a long-lived Docker socket would deadlock
    /// after enough traffic — which is about the worst possible failure to
    /// have to diagnose.
    #[test]
    fn accounting_survives_the_counters_wrapping() {
        let mut credit = Credit::new();
        credit.sent(u32::MAX - 100);
        credit.observe(1000, u32::MAX - 100);
        assert_eq!(credit.available(), 1000);

        // Push both past the wrap.
        credit.sent(500);
        assert_eq!(
            credit.available(),
            500,
            "500 bytes in flight across the wrap"
        );

        credit.observe(1000, credit.tx_cnt);
        assert_eq!(credit.available(), 1000);
    }

    #[test]
    fn a_peer_reporting_more_consumed_than_we_sent_stalls_safely() {
        // Should not happen; a malicious or broken driver can do it anyway, and
        // the answer must not be an enormous apparent credit.
        let mut credit = Credit::new();
        credit.observe(1000, 5000);
        credit.sent(10);
        assert_eq!(credit.available(), 0);
    }

    #[test]
    fn windows_are_full_while_the_budget_lasts_and_least_after() {
        let mut budget = Budget::new(u64::from(BUF_ALLOC) * 2);
        assert_eq!(budget.open(), BUF_ALLOC);
        assert_eq!(budget.open(), BUF_ALLOC);
        assert_eq!(budget.open(), MIN_WINDOW, "the least, however tight");
        assert_eq!(
            budget.granted(),
            u64::from(BUF_ALLOC) * 2 + u64::from(MIN_WINDOW)
        );
    }

    #[test]
    fn a_window_cut_to_the_least_grows_back_by_doubling_into_what_is_free() {
        let mut budget = Budget::new(u64::from(BUF_ALLOC));
        let full = budget.open();
        let least = budget.shrink(full);
        assert_eq!(least, MIN_WINDOW);
        assert_eq!(budget.granted(), u64::from(MIN_WINDOW));
        let mut window = least;
        let mut steps = 0;
        while window < BUF_ALLOC {
            let next = budget.grow(window);
            assert_eq!(next, window * 2);
            window = next;
            steps += 1;
        }
        assert_eq!(steps, 6, "64 KiB to 4 MiB");
        assert_eq!(budget.grow(window), BUF_ALLOC, "never past a full window");
        assert_eq!(budget.granted(), u64::from(BUF_ALLOC));
    }

    #[test]
    fn growth_takes_only_what_the_budget_has_free() {
        let mut budget = Budget::new(u64::from(BUF_ALLOC) + 3 * u64::from(MIN_WINDOW));
        let _hog = budget.open();
        let small = budget.open();
        assert_eq!(small, 3 * MIN_WINDOW);
        assert_eq!(
            budget.grow(small),
            3 * MIN_WINDOW,
            "nothing free, nothing grown"
        );
        budget.close(BUF_ALLOC);
        assert_eq!(budget.grow(small), 6 * MIN_WINDOW);
    }

    #[test]
    fn our_own_consumed_count_is_what_we_publish() {
        let mut credit = Credit::new();
        credit.consumed(1234);
        assert_eq!(credit.fwd_cnt(), 1234);
        assert_eq!(credit.buf_alloc(), BUF_ALLOC);
    }
}
