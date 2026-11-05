//! Defer side-button taps only while Bluetooth is connected, so a swipe has
//! exactly one effect and never changes volume or toggles playback midway.
use crate::runtime::DeviceEvent;
use std::time::{Duration, Instant};

const SWIPE_GAP: Duration = Duration::from_millis(300);

#[derive(Debug)]
pub enum Action {
    Button(DeviceEvent),
    Playback(&'static str),
}

#[derive(Default)]
pub struct Gestures {
    bluetooth: bool,
    pending: Vec<DeviceEvent>,
    deadline: Option<Instant>,
}

fn button(event: &DeviceEvent) -> Option<&str> {
    match event {
        DeviceEvent::Touch { button, action, .. } | DeviceEvent::Button { button, action, .. }
            if action == "pressed" =>
        {
            Some(button)
        }
        _ => None,
    }
}

impl Gestures {
    fn mode(&mut self, bluetooth: bool) {
        if bluetooth != self.bluetooth {
            // A disconnected swipe must not turn its middle touch into a call.
            self.pending.clear();
            self.deadline = None;
            self.bluetooth = bluetooth;
        }
    }

    pub fn expire(&mut self, now: Instant, bluetooth: bool) -> Vec<Action> {
        self.mode(bluetooth);
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.flush()
        } else {
            Vec::new()
        }
    }

    fn flush(&mut self) -> Vec<Action> {
        self.deadline = None;
        self.pending
            .drain(..)
            .map(|event| {
                if button(&event) == Some("center") {
                    Action::Playback("toggle")
                } else {
                    Action::Button(event)
                }
            })
            .collect()
    }

    pub fn push(&mut self, event: DeviceEvent, now: Instant, bluetooth: bool) -> Vec<Action> {
        let mut actions = self.expire(now, bluetooth);
        let Some(key) = button(&event) else {
            return actions;
        };
        if !bluetooth {
            actions.push(Action::Button(event));
            return actions;
        }
        if matches!(event, DeviceEvent::Touch { .. }) {
            let first = self.pending.first().and_then(button);
            if self.pending.len() == 1 && key == "center" {
                self.pending.push(event);
                self.deadline = Some(now + SWIPE_GAP);
                return actions;
            }
            if self.pending.len() == 2
                && ((first == Some("volume_down") && key == "volume_up")
                    || (first == Some("volume_up") && key == "volume_down"))
            {
                let command = if key == "volume_up" {
                    "next"
                } else {
                    "previous"
                };
                self.pending.clear();
                self.deadline = None;
                actions.push(Action::Playback(command));
                return actions;
            }
            actions.extend(self.flush());
            if key == "volume_up" || key == "volume_down" {
                self.pending.push(event);
                self.deadline = Some(now + SWIPE_GAP);
                return actions;
            }
        } else {
            actions.extend(self.flush());
        }
        actions.push(if key == "center" {
            Action::Playback("toggle")
        } else {
            Action::Button(event)
        });
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn touch(key: &str) -> DeviceEvent {
        DeviceEvent::Touch {
            button: key.into(),
            action: "pressed".into(),
            key_code: 0,
            key_name: String::new(),
            at_ms: 0,
        }
    }
    #[test]
    fn both_swipes_suppress_all_intermediate_taps() {
        for (keys, command) in [
            (["volume_down", "center", "volume_up"], "next"),
            (["volume_up", "center", "volume_down"], "previous"),
        ] {
            let mut gestures = Gestures::default();
            let now = Instant::now();
            assert!(gestures.push(touch(keys[0]), now, true).is_empty());
            assert!(
                gestures
                    .push(touch(keys[1]), now + Duration::from_millis(100), true)
                    .is_empty()
            );
            let actions = gestures.push(touch(keys[2]), now + Duration::from_millis(200), true);
            assert!(matches!(&actions[..], [Action::Playback(value)] if *value == command));
            assert!(
                gestures
                    .expire(now + Duration::from_secs(1), true)
                    .is_empty()
            );
        }
    }
    #[test]
    fn taps_and_slow_sequences_remain_regular_buttons() {
        let mut gestures = Gestures::default();
        let now = Instant::now();
        assert!(gestures.push(touch("volume_up"), now, true).is_empty());
        let actions = gestures.push(touch("center"), now + Duration::from_millis(400), true);
        assert!(matches!(
            &actions[..],
            [Action::Button(_), Action::Playback("toggle")]
        ));
        assert!(matches!(
            &gestures.push(touch("center"), now, false)[..],
            [Action::Button(_)]
        ));
        assert!(matches!(
            &gestures.push(touch("volume_up"), now, false)[..],
            [Action::Button(_)]
        ));
    }
    #[test]
    fn disconnect_cancels_partial_swipe_without_starting_agent() {
        let mut gestures = Gestures::default();
        let now = Instant::now();
        gestures.push(touch("volume_down"), now, true);
        gestures.push(touch("center"), now + Duration::from_millis(100), true);
        assert!(
            gestures
                .expire(now + Duration::from_secs(1), false)
                .is_empty()
        );
    }
}
