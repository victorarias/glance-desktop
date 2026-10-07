//! Correlate the native clipboard capture shortcut with a completed selection.
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::{Monitor, copy_image, load_enabled, save_enabled, snapshot};

fn consistent_snapshot<T>(
    generation: i64,
    current: impl Fn() -> i64,
    read: impl FnOnce() -> Result<T, String>,
) -> Result<Option<T>, String> {
    if current() != generation {
        return Ok(None);
    }
    let result = read();
    if current() != generation {
        return Ok(None);
    }
    result.map(Some)
}

// Selection itself has no time limit. This bounds only the clipboard handoff
// after the user finishes, so a failed capture cannot arm a future image copy.
const HANDOFF_LIMIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Tracker {
    next_token: u64,
    request: Option<Request>,
}
struct Request {
    token: u64,
    baseline: i64,
    mouse_down: bool,
    completed: Option<Instant>,
    generation: Option<i64>,
}
impl Tracker {
    fn shortcut(&mut self, baseline: i64) {
        self.next_token += 1;
        self.request = Some(Request {
            token: self.next_token,
            baseline,
            mouse_down: false,
            completed: None,
            generation: None,
        });
    }
    fn cancel(&mut self) {
        self.request = None;
    }
    fn mouse_down(&mut self) {
        if let Some(request) = &mut self.request {
            if request.completed.is_some() {
                self.cancel();
            } else {
                request.mouse_down = true;
            }
        }
    }
    fn mouse_up(&mut self, now: Instant) {
        if let Some(request) = &mut self.request
            && request.mouse_down
        {
            request.completed = Some(now);
        }
    }
    /// Claim the first new generation once. Data may still be promised by its
    /// owner; a worker waits for it without following a later clipboard owner.
    fn poll(&mut self, generation: i64, now: Instant) -> Option<(u64, i64)> {
        let request = self.request.as_mut()?;
        if request
            .completed
            .is_some_and(|at| now.duration_since(at) >= HANDOFF_LIMIT)
        {
            self.cancel();
            return None;
        }
        if generation == request.baseline {
            return None;
        }
        // Clipboard changes before a completion gesture are unrelated.
        if request.completed.is_none() {
            self.cancel();
            return None;
        }
        if request
            .generation
            .is_some_and(|expected| expected != generation)
        {
            self.cancel();
            return None;
        }
        if request.generation.is_some() {
            return None;
        }
        request.generation = Some(generation);
        Some((request.token, generation))
    }
    fn accepts(&self, token: u64, generation: i64, now: Instant) -> bool {
        self.request.as_ref().is_some_and(|request| {
            request.token == token
                && request.generation == Some(generation)
                && request
                    .completed
                    .is_some_and(|at| now.duration_since(at) < HANDOFF_LIMIT)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_can_take_time_but_handoff_cannot() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.shortcut(7);
        assert_eq!(tracker.poll(7, now + Duration::from_secs(120)), None);
        tracker.mouse_down();
        tracker.mouse_up(now + Duration::from_secs(120));
        assert_eq!(
            tracker.poll(8, now + Duration::from_secs(121)),
            Some((1, 8))
        );
        assert_eq!(tracker.poll(8, now + Duration::from_secs(122)), None);
        assert!(tracker.accepts(1, 8, now + Duration::from_secs(122)));
        assert!(
            !tracker.accepts(1, 8, now + Duration::from_secs(131)),
            "deadline enforced without another poll"
        );
        assert_eq!(tracker.poll(8, now + Duration::from_secs(131)), None);
        assert!(!tracker.accepts(1, 8, now + Duration::from_secs(131)));
    }
    #[test]
    fn cancellation_and_replacement_reject_old_images() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.shortcut(1);
        tracker.mouse_up(now);
        assert_eq!(tracker.poll(2, now), None);
        tracker.shortcut(2);
        tracker.mouse_down();
        tracker.mouse_up(now);
        assert_eq!(tracker.poll(3, now), Some((2, 3)));
        tracker.cancel();
        assert!(!tracker.accepts(2, 3, now));
        assert_eq!(tracker.poll(4, now), None);
        tracker.shortcut(4);
        tracker.mouse_down();
        tracker.mouse_up(now);
        assert_eq!(tracker.poll(5, now), Some((3, 5)));
        assert_eq!(tracker.poll(6, now), None);
        assert!(!tracker.accepts(3, 5, now));
        tracker.shortcut(6);
        assert!(!tracker.accepts(3, 5, now));
    }
    #[test]
    fn snapshot_does_not_follow_a_new_clipboard_owner() {
        let generation = std::cell::Cell::new(4);
        assert_eq!(
            consistent_snapshot(
                3,
                || generation.get(),
                || panic!("must not read stale owner")
            ),
            Ok(None::<u8>)
        );
        assert_eq!(
            consistent_snapshot(
                4,
                || generation.get(),
                || {
                    generation.set(5);
                    Ok(42)
                }
            ),
            Ok(None)
        );
        assert_eq!(
            consistent_snapshot(5, || generation.get(), || Ok(99)),
            Ok(Some(99))
        );
        assert_eq!(
            consistent_snapshot(
                5,
                || generation.get(),
                || Err::<u8, _>("not an image".into())
            ),
            Err("not an image".into())
        );
    }
}
