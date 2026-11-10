//! The hop slot of a **listening** node (US 915, FCC §15.247 frequency hopping): when in a slot
//! the gateway sends a frame nobody asked for, and how the node cuts its listening so that it
//! hears it (docs/firmware/radio.md, "Node classes" and "In the product apps").
//!
//! A sleeping node hears its gateway only in the slot of its own uplink, right after the ACK. A
//! listening node (mains powered) keeps its receiver on the schedule's current channel between
//! its sends, and the gateway sends its queued downlink in the current slot, without waiting for
//! an uplink. Two things can go wrong there, and the plan rules both out:
//!
//! - **The downlink meets an uplink.** Every node with something to send transmits right after
//!   the slot's beacon. So the gateway listens first and sends at [`SEND_AT_MS`] into the slot:
//!   an uplink of that slot has long started by then (it is received to its end and ACKed, and
//!   the downlink waits for the next slot).
//! - **The downlink meets a gap in the node's receiving.** The node receives in slices (its loop
//!   looks at the button, the timers and the cable between two), and the receiver is off from
//!   the end of one slice until the next is armed: about a millisecond, and up to ten or so when
//!   another task holds the executor (an LED strip's frame). A frame whose preamble falls into
//!   that is lost. Both ends hold the slot clock, so no slice ends near the send time
//!   ([`listen_slice_ms`]): one slice spans it, from [`LEAD_MS`] before to [`TAIL_MS`] after.
//!
//! The margins are uneven on purpose. The node's slot clock runs a little behind the gateway's
//! (it takes the beacon's arrival for the boundary, and reads the clock only once its task
//! runs), and arming is late by the same kind of delay, so both errors eat the time before the
//! frame; only the gateway's own delay before it transmits eats the time after it.
//!
//! The plan adds no channel and no transmission off the schedule. The downlink goes out on the
//! channel of the slot that is current when the host queued it, inside that slot, so the
//! schedule's even channel use and its bound on a channel's occupancy hold as they are (the
//! firmware checks at compile time that a full-length exchange sent at `SEND_AT_MS` ends inside
//! its slot, and that a node that missed the slot's beacon is listening again by then).
//!
//! Times are in ms from the slot's boundary, where the beacon goes out; the caller owns the
//! clock.

/// When the hop master sends a frame of its own to a listening node: this long into the slot.
/// Late enough that an uplink following the beacon has started (the beacon takes about 18 ms, a
/// node transmits within a few ms of hearing it) and that a node that missed the beacon has left
/// its beacon window (it closes about 90 ms in) and armed a slice; early enough that a
/// full-length frame, the turnaround and its ACK end before the master's cut.
pub const SEND_AT_MS: u32 = 125;

/// No receive slice ends within this long before [`SEND_AT_MS`]: the receiver has been armed
/// that long (less the loop's time between two slices) when the master's frame starts.
pub const LEAD_MS: u32 = 40;

/// Nor within this long after it: the frame's sync word, which keeps the receiver on to the
/// frame's end, comes about 6 ms after the send time.
pub const TAIL_MS: u32 = 30;

/// A listening node's receive slice away from the send time: about the longest a local event (a
/// button, a report, the cable) waits for the radio loop, as on EU 868. The slice over the send
/// time is `LEAD_MS + TAIL_MS` long.
pub const SLICE_MS: u32 = 50;

/// The shortest slice worth arming the receiver for: a slice that would end sooner runs on to
/// the next slice end.
pub const MIN_SLICE_MS: u32 = 10;

/// How long a listening node's next receive slice lasts, `into_ms` into the current slot whose
/// listening ends at `active_ms`, or `None` when the slot is over (less than [`MIN_SLICE_MS`]
/// left): the node hops, and listens for the next slot's beacon.
///
/// `active_ms` is the hop master's cut: it starts no exchange that would not end by then, so
/// nothing reaches the node later in the slot, and what is left of the slot is the node's margin.
/// Its loop must come back for the hop before the slot ends, or it finds itself in the next slot
/// with the receiver still on this one's channel; with the cut at 250 ms of a 300 ms slot the
/// loop's work between two slices (a few ms as a rule) has 50 ms for it.
///
/// Slices end at times fixed in the slot, whenever the node began (after the beacon, after a
/// beacon it missed, after a send of its own): at `SEND_AT_MS - LEAD_MS` and every [`SLICE_MS`]
/// before it, at `SEND_AT_MS + TAIL_MS` and every `SLICE_MS` after it, and never between the
/// two. `hold` asks for one slice to the end of the slot's listening instead: the gateway's ACK
/// announced a downlink, which follows within the slot at a time of its own.
#[must_use]
pub fn listen_slice_ms(into_ms: u32, active_ms: u32, hold: bool) -> Option<u32> {
    /// The last slice end before the send time, and the first after it.
    const BEFORE: u32 = SEND_AT_MS - LEAD_MS;
    const AFTER: u32 = SEND_AT_MS + TAIL_MS;
    if into_ms.saturating_add(MIN_SLICE_MS) > active_ms {
        return None;
    }
    if hold {
        return Some(active_ms - into_ms);
    }
    let earliest = into_ms + MIN_SLICE_MS;
    let end = if earliest <= BEFORE {
        BEFORE - (BEFORE - earliest) / SLICE_MS * SLICE_MS
    } else if earliest <= AFTER {
        AFTER
    } else {
        AFTER + (earliest - AFTER).div_ceil(SLICE_MS) * SLICE_MS
    };
    Some(end.min(active_ms) - into_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The firmware's cut in a 300 ms slot (its guard of 10 ms and the master's loop budget of
    /// 40 ms before the next boundary): where a node's listening ends.
    const ACTIVE_MS: u32 = 250;
    /// The firmware's beacon window of a tracking node closes this long into the slot (it opens a
    /// guard early and lasts 100 ms, plus a millisecond or two of drift slack).
    const BEACON_WINDOW_END_MS: u32 = 92;
    /// The longest a node's loop is assumed to take between two slices: the receiver's re-arming
    /// and one frame of an LED strip holding the executor.
    const LOOP_MS: u32 = 12;

    /// Every slice is at least `MIN_SLICE_MS` long, at most the slice over the send time and a
    /// minimal one, and ends inside the slot's listening; the slot is over exactly when less than
    /// a minimal slice is left.
    #[test]
    fn slices_are_bounded_and_stay_inside_the_slot() {
        for into in 0..ACTIVE_MS + 50 {
            match listen_slice_ms(into, ACTIVE_MS, false) {
                Some(ms) => {
                    assert!(into + MIN_SLICE_MS <= ACTIVE_MS, "into {into}");
                    assert!(ms >= MIN_SLICE_MS, "into {into}: {ms} ms");
                    assert!(ms < LEAD_MS + TAIL_MS + MIN_SLICE_MS, "into {into}: {ms} ms");
                    assert!(into + ms <= ACTIVE_MS, "into {into}: {ms} ms");
                }
                None => assert!(into + MIN_SLICE_MS > ACTIVE_MS, "into {into}"),
            }
        }
        assert_eq!(listen_slice_ms(u32::MAX, ACTIVE_MS, false), None);
        assert_eq!(listen_slice_ms(0, 0, false), None);
        // A shorter slot than the plan: the slices still end inside it.
        for into in 0..100 {
            if let Some(ms) = listen_slice_ms(into, 100, false) {
                assert!(ms >= MIN_SLICE_MS && into + ms <= 100, "into {into}: {ms} ms");
            }
        }
    }

    /// Whenever the node starts slicing (right after the beacon, after a beacon it missed, after
    /// a send of its own) and however long its loop takes between two slices, no slice ends
    /// around the send time: the slice the gateway's frame starts in was armed at least a
    /// minimal slice before it (most of `LEAD_MS` when the node began in time) and lasts
    /// `TAIL_MS` past it (a frame in flight at a slice's end is received to its end).
    #[test]
    fn no_slice_ends_around_the_gateways_send() {
        for start in 0..=SEND_AT_MS - MIN_SLICE_MS {
            for gap in 0..=LOOP_MS {
                let mut at = start;
                loop {
                    let ms = listen_slice_ms(at, ACTIVE_MS, false).expect("inside the slot");
                    let end = at + ms;
                    assert!(
                        end <= SEND_AT_MS - LEAD_MS || end >= SEND_AT_MS + TAIL_MS,
                        "start {start}, gap {gap}: a slice ends at {end}"
                    );
                    if end > SEND_AT_MS {
                        assert!(
                            at + MIN_SLICE_MS <= SEND_AT_MS,
                            "start {start}, gap {gap}: armed at {at}"
                        );
                        if start <= SEND_AT_MS - LEAD_MS {
                            assert!(
                                at + LEAD_MS <= SEND_AT_MS + LOOP_MS,
                                "start {start}, gap {gap}: armed at {at}"
                            );
                        }
                        break;
                    }
                    at = end + gap;
                }
            }
        }
        // The two starts that matter: the beacon heard (about 20 ms in), and missed.
        assert_eq!(listen_slice_ms(20, ACTIVE_MS, false), Some(15));
        assert_eq!(listen_slice_ms(36, ACTIVE_MS, false), Some(49));
        assert_eq!(listen_slice_ms(86, ACTIVE_MS, false), Some(69));
        assert_eq!(listen_slice_ms(BEACON_WINDOW_END_MS, ACTIVE_MS, false), Some(63));
        // A node that missed the beacon is out of its beacon window, with its loop's time to arm
        // a slice, before the send.
        const { assert!(BEACON_WINDOW_END_MS + LOOP_MS + MIN_SLICE_MS <= SEND_AT_MS) };
    }

    /// A node walks the whole slot in slices and then hops: nothing shorter than a minimal slice
    /// is armed at the slot's end, and a slice never runs past the slot's listening.
    #[test]
    fn a_slot_is_listened_to_its_end_then_the_node_hops() {
        let (mut at, mut slices) = (20, 0);
        while let Some(ms) = listen_slice_ms(at, ACTIVE_MS, false) {
            at += ms;
            slices += 1;
        }
        assert_eq!(slices, 5, "the slices end at 35, 85, 155, 205 and at the cut");
        assert_eq!(at, ACTIVE_MS, "the last slice ends with the slot's listening");
        // Before the send time the ends are a slice apart, counted back from the lead.
        assert_eq!(listen_slice_ms(0, ACTIVE_MS, false), Some(35));
        assert_eq!(listen_slice_ms(25, ACTIVE_MS, false), Some(10));
        assert_eq!(listen_slice_ms(26, ACTIVE_MS, false), Some(59));
        // A slice that would end past the slot's listening ends with it.
        assert_eq!(listen_slice_ms(206, ACTIVE_MS, false), Some(44));
        assert_eq!(
            listen_slice_ms(ACTIVE_MS - MIN_SLICE_MS, ACTIVE_MS, false),
            Some(MIN_SLICE_MS)
        );
        assert_eq!(
            listen_slice_ms(ACTIVE_MS - MIN_SLICE_MS + 1, ACTIVE_MS, false),
            None
        );
    }

    /// An announced downlink: one slice to the end of the slot's listening, from wherever the
    /// node is.
    #[test]
    fn a_hold_listens_to_the_end_of_the_slot() {
        assert_eq!(listen_slice_ms(0, ACTIVE_MS, true), Some(ACTIVE_MS));
        assert_eq!(listen_slice_ms(104, ACTIVE_MS, true), Some(146));
        assert_eq!(
            listen_slice_ms(ACTIVE_MS - MIN_SLICE_MS, ACTIVE_MS, true),
            Some(MIN_SLICE_MS)
        );
        assert_eq!(
            listen_slice_ms(ACTIVE_MS - 1, ACTIVE_MS, true),
            None,
            "the slot is over: hop"
        );
        assert_eq!(listen_slice_ms(ACTIVE_MS + 7, ACTIVE_MS, true), None);
    }
}
