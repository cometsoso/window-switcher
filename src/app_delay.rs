use std::time::{Duration, Instant};

/// UI-thread state only; dropping it cancels the logical timer. IDs must never be
/// reused, because KillTimer does not remove already queued WM_TIMER messages.
pub struct AppDelay {
    pub id: usize,
    deadline: Instant,
    immediate: bool,
    visible: bool,
    released: bool,
    timer_pending: bool,
    navigation: Vec<bool>,
}

impl AppDelay {
    pub fn new(id: usize, now: Instant, delay: u32) -> Self {
        Self {
            id,
            deadline: now + Duration::from_millis(delay.into()),
            immediate: delay == 0,
            visible: false,
            released: false,
            timer_pending: delay != 0,
            navigation: vec![],
        }
    }

    pub fn release(&mut self) {
        self.released = true;
        self.timer_pending = false;
    }

    pub fn released(&self) -> bool {
        self.released
    }

    pub fn show(&mut self, now: Instant, ready: bool) -> bool {
        if !ready || self.released || self.visible || (!self.immediate && now < self.deadline) {
            return false;
        }
        self.visible = true;
        self.timer_pending = false;
        true
    }

    pub fn accepts_mouse(&self) -> bool {
        self.visible && !self.released
    }

    pub fn navigate(&mut self, reverse: bool) {
        if !self.released {
            self.navigation.push(reverse);
            self.immediate = true;
        }
    }

    pub fn take_navigation(&mut self) -> Vec<bool> {
        std::mem::take(&mut self.navigation)
    }

    pub fn timer_failed(&mut self) {
        self.immediate = true;
    }

    pub fn remaining_ms(&self, now: Instant) -> u32 {
        if self.immediate || self.released || self.visible {
            return 0;
        }
        self.deadline
            .saturating_duration_since(now)
            .as_nanos()
            .div_ceil(1_000_000) as u32
    }

    pub fn timer(&mut self, id: usize) -> bool {
        if id != self.id || !self.timer_pending {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_before_deadline_never_shows_even_after_build_finishes() {
        let now = Instant::now();
        let mut session = AppDelay::new(10, now, 150);
        session.release();
        assert!(!session.show(now + Duration::from_millis(200), true));
        assert!(session.released());
        assert!(!session.accepts_mouse());
    }

    #[test]
    fn deadline_starts_before_build_and_shows_only_once() {
        let now = Instant::now();
        let mut session = AppDelay::new(10, now, 150);
        assert!(!session.show(now + Duration::from_millis(149), true));
        assert!(!session.show(now + Duration::from_millis(150), false));
        assert!(session.show(now + Duration::from_millis(200), true));
        assert!(!session.show(now + Duration::from_millis(201), true));
        assert!(session.accepts_mouse());
    }

    #[test]
    fn second_main_key_reverse_or_arrow_requests_immediate_display() {
        let now = Instant::now();
        for reverse in [false, true] {
            let mut session = AppDelay::new(10, now, 150);
            session.navigate(reverse);
            assert_eq!(session.take_navigation(), vec![reverse]);
            assert!(session.show(now, true));
        }
    }

    #[test]
    fn zero_delay_and_timer_failure_show_immediately() {
        let now = Instant::now();
        assert!(AppDelay::new(10, now, 0).show(now, true));
        let mut session = AppDelay::new(11, now, 150);
        session.timer_failed();
        assert!(session.show(now, true));
    }

    #[test]
    fn canceled_or_previous_round_timer_cannot_show_new_round() {
        let now = Instant::now();
        let mut session = AppDelay::new(11, now, 150);
        assert!(!session.timer(10));
        assert!(!session.show(now, true));
        assert!(session.timer(11));
        assert!(session.show(now + Duration::from_millis(150), true));
        assert!(!session.timer(11));
        let mut next = AppDelay::new(12, now, 150);
        assert!(!next.timer(11));
        assert!(!next.accepts_mouse());
        assert!(!next.show(now, true));
    }

    #[test]
    fn early_timer_is_rearmed_instead_of_losing_the_reveal() {
        let now = Instant::now();
        let mut session = AppDelay::new(10, now, 150);
        assert!(session.timer(10));
        assert_eq!(session.remaining_ms(now + Duration::from_millis(149)), 1);
        assert!(!session.show(now + Duration::from_millis(149), true));
        assert!(session.timer(10));
        assert_eq!(session.remaining_ms(now + Duration::from_millis(150)), 0);
        assert!(session.show(now + Duration::from_millis(150), true));
    }
}
