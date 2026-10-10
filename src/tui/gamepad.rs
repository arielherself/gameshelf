//! Gamepad input.
//!
//! A thread reads controllers through gilrs (evdev, with SDL's controller
//! database) and turns them into [`Pad`] presses. Buttons are positional
//! (bottom, right, left, top), so every controller behaves the same; only the
//! labels shown in hints depend on the controller family. The D-pad and the
//! left stick repeat while held, like arrow keys.

use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use gilrs::{Axis, Button, EventType, Gilrs};
use ratatui::style::Color;

/// A controller input, by position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pad {
    Up,
    Down,
    Left,
    Right,
    /// Bottom face button (A on Xbox, ✕ on PlayStation, B on Nintendo).
    South,
    East,
    West,
    North,
    Lb,
    Rb,
    Start,
    Select,
}

impl Pad {
    fn is_direction(self) -> bool {
        matches!(self, Pad::Up | Pad::Down | Pad::Left | Pad::Right)
    }
}

/// Whose button labels to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Style {
    /// Also the default for anything that isn't recognisably Sony or Nintendo.
    #[default]
    Xbox,
    PlayStation,
    Nintendo,
}

impl Style {
    pub fn detect(vendor: Option<u16>, name: &str) -> Self {
        let name = name.to_lowercase();
        match vendor {
            Some(0x054c) => Style::PlayStation,
            Some(0x057e) => Style::Nintendo,
            _ if ["dualsense", "dualshock", "playstation"].iter().any(|n| name.contains(n)) => {
                Style::PlayStation
            }
            _ if ["nintendo", "switch", "joy-con"].iter().any(|n| name.contains(n)) => Style::Nintendo,
            _ => Style::Xbox,
        }
    }

    /// Label and colour for a button in hints.
    pub fn label(self, pad: Pad) -> (&'static str, Option<Color>) {
        use Style::*;
        let face = |xbox, ps, nin, xc: (u8, u8, u8), pc: (u8, u8, u8)| match self {
            Xbox => (xbox, Some(Color::Rgb(xc.0, xc.1, xc.2))),
            PlayStation => (ps, Some(Color::Rgb(pc.0, pc.1, pc.2))),
            Nintendo => (nin, None),
        };
        match pad {
            Pad::South => face("A", "✕", "B", (130, 200, 100), (124, 178, 232)),
            Pad::East => face("B", "○", "A", (235, 100, 100), (255, 110, 110)),
            Pad::West => face("X", "□", "Y", (90, 150, 240), (217, 150, 201)),
            Pad::North => face("Y", "△", "X", (240, 200, 70), (64, 214, 170)),
            Pad::Lb => (["LB", "L1", "L"][self as usize], None),
            Pad::Rb => (["RB", "R1", "R"][self as usize], None),
            Pad::Start => (["≡", "≡", "+"][self as usize], None),
            Pad::Select => (["⧉", "⧉", "−"][self as usize], None),
            Pad::Up | Pad::Down | Pad::Left | Pad::Right => ("✛", None),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Style::Xbox => "Xbox",
            Style::PlayStation => "PlayStation",
            Style::Nintendo => "Nintendo",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PadEvent {
    Press(Pad),
    Connected { name: String, style: Style },
    Disconnected,
}

fn map_button(b: Button) -> Option<Pad> {
    Some(match b {
        Button::DPadUp => Pad::Up,
        Button::DPadDown => Pad::Down,
        Button::DPadLeft => Pad::Left,
        Button::DPadRight => Pad::Right,
        Button::South => Pad::South,
        Button::East => Pad::East,
        Button::West => Pad::West,
        Button::North => Pad::North,
        Button::LeftTrigger => Pad::Lb,
        Button::RightTrigger => Pad::Rb,
        Button::Start => Pad::Start,
        Button::Select => Pad::Select,
        _ => return None,
    })
}

const FIRST_REPEAT: Duration = Duration::from_millis(320);
const NEXT_REPEAT: Duration = Duration::from_millis(90);

/// Auto-repeat for a held direction.
#[derive(Debug, Default)]
struct Repeater {
    held: Option<(Pad, Instant)>,
}

impl Repeater {
    fn press(&mut self, pad: Pad, now: Instant) {
        self.held = Some((pad, now + FIRST_REPEAT));
    }

    fn release(&mut self, pad: Pad) {
        if self.held.is_some_and(|(p, _)| p == pad) {
            self.held = None;
        }
    }

    /// The direction to repeat now, if one is due.
    fn due(&mut self, now: Instant) -> Option<Pad> {
        let (pad, next) = self.held.as_mut()?;
        if now >= *next {
            *next = now + NEXT_REPEAT;
            Some(*pad)
        } else {
            None
        }
    }

    fn wait(&self, now: Instant) -> Option<Duration> {
        self.held.map(|(_, next)| next.saturating_duration_since(now))
    }
}

/// Turns a 2D axis (stick or hat) into direction presses, with hysteresis
/// so a stick resting near the threshold doesn't flicker.
#[derive(Debug, Default)]
struct Stick {
    x: f32,
    y: f32,
    dir: Option<Pad>,
}

const PRESS_AT: f32 = 0.6;
const RELEASE_AT: f32 = 0.35;

impl Stick {
    /// Update one axis (`y` positive is up). Returns `(released, pressed)`.
    fn update(&mut self, x: Option<f32>, y: Option<f32>) -> (Option<Pad>, Option<Pad>) {
        if let Some(x) = x {
            self.x = x;
        }
        if let Some(y) = y {
            self.y = y;
        }
        let (ax, ay) = (self.x.abs(), self.y.abs());
        let candidate = if ax >= ay {
            (ax > 0.0).then_some(if self.x > 0.0 { Pad::Right } else { Pad::Left })
        } else {
            Some(if self.y > 0.0 { Pad::Up } else { Pad::Down })
        };
        let strength = ax.max(ay);
        let new = match self.dir {
            // Keep holding until the stick drops below the release threshold.
            Some(d) if Some(d) == candidate && strength > RELEASE_AT => Some(d),
            _ if strength >= PRESS_AT => candidate,
            _ => None,
        };
        let old = self.dir;
        self.dir = new;
        if old == new { (None, None) } else { (old, new) }
    }
}

/// Start reading controllers. Returns quietly if gamepads can't be read
/// (e.g. no permission for /dev/input).
pub fn spawn(tx: Sender<PadEvent>) {
    thread::spawn(move || {
        let Ok(mut gilrs) = Gilrs::new() else { return };
        for (_, gp) in gilrs.gamepads() {
            let style = Style::detect(gp.vendor_id(), gp.name());
            if tx.send(PadEvent::Connected { name: gp.name().to_string(), style }).is_err() {
                return;
            }
        }
        let mut repeat = Repeater::default();
        let mut left_stick = Stick::default();
        let mut hat = Stick::default();
        loop {
            let wait = repeat.wait(Instant::now()).unwrap_or(Duration::from_millis(500));
            let mut out = Vec::new();
            if let Some(ev) = gilrs.next_event_blocking(Some(wait)) {
                let now = Instant::now();
                match ev.event {
                    EventType::ButtonPressed(b, _) => {
                        if let Some(p) = map_button(b) {
                            out.push(PadEvent::Press(p));
                            if p.is_direction() {
                                repeat.press(p, now);
                            }
                        }
                    }
                    EventType::ButtonReleased(b, _) => {
                        if let Some(p) = map_button(b) {
                            repeat.release(p);
                        }
                    }
                    EventType::AxisChanged(axis, v, _) => {
                        let (stick, x, y) = match axis {
                            Axis::LeftStickX => (&mut left_stick, Some(v), None),
                            Axis::LeftStickY => (&mut left_stick, None, Some(v)),
                            Axis::DPadX => (&mut hat, Some(v), None),
                            Axis::DPadY => (&mut hat, None, Some(v)),
                            _ => continue,
                        };
                        let (released, pressed) = stick.update(x, y);
                        if let Some(r) = released {
                            repeat.release(r);
                        }
                        if let Some(p) = pressed {
                            out.push(PadEvent::Press(p));
                            repeat.press(p, now);
                        }
                    }
                    EventType::Connected => {
                        let gp = gilrs.gamepad(ev.id);
                        let style = Style::detect(gp.vendor_id(), gp.name());
                        out.push(PadEvent::Connected { name: gp.name().to_string(), style });
                    }
                    EventType::Disconnected => {
                        repeat = Repeater::default();
                        out.push(PadEvent::Disconnected);
                    }
                    _ => {}
                }
            }
            if let Some(p) = repeat.due(Instant::now()) {
                out.push(PadEvent::Press(p));
            }
            for e in out {
                if tx.send(e).is_err() {
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_families() {
        assert_eq!(
            Style::detect(Some(0x054c), "Sony Interactive Entertainment Wireless Controller"),
            Style::PlayStation
        );
        assert_eq!(Style::detect(None, "DualSense Wireless Controller"), Style::PlayStation);
        assert_eq!(Style::detect(Some(0x057e), "Pro Controller"), Style::Nintendo);
        assert_eq!(Style::detect(Some(0x045e), "Xbox Wireless Controller"), Style::Xbox);
        // Anything else is treated as an Xbox-compatible pad.
        assert_eq!(Style::detect(Some(0x3537), "GameSir Nova 2 Lite"), Style::Xbox);
    }

    #[test]
    fn labels_follow_the_family() {
        assert_eq!(Style::Xbox.label(Pad::South).0, "A");
        assert_eq!(Style::PlayStation.label(Pad::South).0, "✕");
        assert_eq!(Style::Nintendo.label(Pad::South).0, "B", "bottom button is B on Nintendo pads");
        assert_eq!(Style::Nintendo.label(Pad::East).0, "A");
        assert_eq!(Style::PlayStation.label(Pad::Rb).0, "R1");
        assert_eq!(Style::Nintendo.label(Pad::Start).0, "+");
    }

    #[test]
    fn repeats_held_directions() {
        let t0 = Instant::now();
        let mut r = Repeater::default();
        assert_eq!(r.due(t0), None);
        r.press(Pad::Down, t0);
        assert_eq!(r.due(t0 + Duration::from_millis(100)), None, "initial delay");
        assert_eq!(r.due(t0 + FIRST_REPEAT), Some(Pad::Down));
        assert_eq!(r.due(t0 + FIRST_REPEAT + Duration::from_millis(10)), None);
        assert_eq!(r.due(t0 + FIRST_REPEAT + NEXT_REPEAT), Some(Pad::Down));
        r.release(Pad::Up);
        assert!(r.held.is_some(), "releasing another button doesn't stop the repeat");
        r.release(Pad::Down);
        assert_eq!(r.due(t0 + Duration::from_secs(5)), None);
    }

    #[test]
    fn stick_has_hysteresis() {
        let mut s = Stick::default();
        assert_eq!(s.update(None, Some(-0.5)), (None, None), "below the press threshold");
        assert_eq!(s.update(None, Some(-0.7)), (None, Some(Pad::Down)));
        assert_eq!(s.update(None, Some(-0.45)), (None, None), "held above the release threshold");
        assert_eq!(s.update(None, Some(-0.2)), (Some(Pad::Down), None));
        assert_eq!(s.update(Some(0.9), Some(0.0)), (None, Some(Pad::Right)));
        // Swinging straight across switches direction.
        assert_eq!(s.update(Some(-0.9), None), (Some(Pad::Right), Some(Pad::Left)));
        assert_eq!(s.update(Some(0.0), None), (Some(Pad::Left), None));
        // A hat reports up as positive y.
        assert_eq!(s.update(None, Some(1.0)), (None, Some(Pad::Up)));
    }
}
