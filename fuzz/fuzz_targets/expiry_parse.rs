//! Interrupt::expiry over arbitrary `expiresAt` text (two candidates split at
//! the first newline). Invariants: no panic; valid timestamps order
//! consistently with expiry (the earlier one is expired whenever the later is).
#![no_main]

use agui_sdk::interrupt::{Expiry, Interrupt};
use libfuzzer_sys::fuzz_target;

fn expiry(text: &str) -> Expiry {
    Interrupt::new("i", "fuzz").with_expires_at(text).expiry()
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (a, b) = text.split_once('\n').unwrap_or((text, text));
    if let (Expiry::At(ta), Expiry::At(tb)) = (expiry(a), expiry(b)) {
        let (early, late, t) = if ta <= tb { (a, b, tb) } else { (b, a, ta) };
        let early = Interrupt::new("e", "fuzz").with_expires_at(early);
        let late = Interrupt::new("l", "fuzz").with_expires_at(late);
        assert!(
            late.is_expired_at(t),
            "an instant at its own expiry is expired"
        );
        assert!(
            early.is_expired_at(t),
            "the earlier expiry must be expired too"
        );
    }
});
