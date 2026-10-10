//! Gamepad input through SDL3.
//!
//! SDL maps every controller it knows onto a standard positional layout
//! (south, east, west, north…), knows what kind of controller it is, and
//! knows what's printed on each face button. A thread runs SDL's event loop
//! and turns gamepad events into [`Pad`] presses; the D-pad and the left
//! stick repeat while held, like arrow keys.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use ratatui::style::Color;
use sdl3::event::Event;
use sdl3::gamepad::{Axis, Button, ButtonLabel, Gamepad, GamepadType};

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

/// Controller family, for the buttons SDL doesn't label (shoulders, start…).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Family {
    /// Also anything SDL doesn't recognise as Sony or Nintendo.
    #[default]
    Xbox,
    PlayStation,
    Nintendo,
}

/// What's printed on a face button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Face {
    A,
    B,
    X,
    Y,
    Cross,
    Circle,
    Square,
    Triangle,
}

/// How to label a controller's buttons in hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub family: Family,
    /// Labels of the south, east, west and north buttons.
    pub faces: [Face; 4],
    /// SDL's controller type, e.g. "PlayStation 5".
    pub kind: &'static str,
}

impl Default for Layout {
    fn default() -> Self {
        Self { family: Family::Xbox, faces: [Face::A, Face::B, Face::X, Face::Y], kind: "Xbox-compatible" }
    }
}

impl Layout {
    fn from_sdl(pad: &Gamepad) -> Self {
        let kind = pad.r#type();
        let family = match kind {
            GamepadType::PS3 | GamepadType::PS4 | GamepadType::PS5 => Family::PlayStation,
            GamepadType::NintendoSwitchPro
            | GamepadType::NintendoSwitchJoyconLeft
            | GamepadType::NintendoSwitchJoyconRight
            | GamepadType::NintendoSwitchJoyconPair => Family::Nintendo,
            _ => Family::Xbox,
        };
        let default = Layout::default().faces;
        let face = |button, fallback| match pad.button_label_for_gamepad_type(button) {
            ButtonLabel::A => Face::A,
            ButtonLabel::B => Face::B,
            ButtonLabel::X => Face::X,
            ButtonLabel::Y => Face::Y,
            ButtonLabel::Cross => Face::Cross,
            ButtonLabel::Circle => Face::Circle,
            ButtonLabel::Square => Face::Square,
            ButtonLabel::Triangle => Face::Triangle,
            ButtonLabel::Unknown => fallback,
        };
        Self {
            family,
            faces: [
                face(Button::South, default[0]),
                face(Button::East, default[1]),
                face(Button::West, default[2]),
                face(Button::North, default[3]),
            ],
            kind: type_name(kind),
        }
    }

    /// Label and colour for a button in hints.
    pub fn label(&self, pad: Pad) -> (&'static str, Option<Color>) {
        let face = |f: Face| {
            let (label, (r, g, b)) = match f {
                Face::A => ("A", (130, 200, 100)),
                Face::B => ("B", (235, 100, 100)),
                Face::X => ("X", (90, 150, 240)),
                Face::Y => ("Y", (240, 200, 70)),
                Face::Cross => ("✕", (124, 178, 232)),
                Face::Circle => ("○", (255, 110, 110)),
                Face::Square => ("□", (217, 150, 201)),
                Face::Triangle => ("△", (64, 214, 170)),
            };
            // Nintendo's letters aren't coloured on the controller.
            (label, (self.family != Family::Nintendo).then_some(Color::Rgb(r, g, b)))
        };
        let by_family = |xbox, ps, nintendo| match self.family {
            Family::Xbox => xbox,
            Family::PlayStation => ps,
            Family::Nintendo => nintendo,
        };
        match pad {
            Pad::South => face(self.faces[0]),
            Pad::East => face(self.faces[1]),
            Pad::West => face(self.faces[2]),
            Pad::North => face(self.faces[3]),
            Pad::Lb => (by_family("LB", "L1", "L"), None),
            Pad::Rb => (by_family("RB", "R1", "R"), None),
            Pad::Start => (by_family("≡", "≡", "+"), None),
            Pad::Select => (by_family("⧉", "⧉", "−"), None),
            Pad::Up | Pad::Down | Pad::Left | Pad::Right => ("✛", None),
        }
    }
}

fn type_name(kind: GamepadType) -> &'static str {
    match kind {
        GamepadType::Xbox360 => "Xbox 360",
        GamepadType::XboxOne => "Xbox One",
        GamepadType::PS3 => "PlayStation 3",
        GamepadType::PS4 => "PlayStation 4",
        GamepadType::PS5 => "PlayStation 5",
        GamepadType::NintendoSwitchPro => "Switch Pro",
        GamepadType::NintendoSwitchJoyconLeft => "Joy-Con (L)",
        GamepadType::NintendoSwitchJoyconRight => "Joy-Con (R)",
        GamepadType::NintendoSwitchJoyconPair => "Joy-Con pair",
        GamepadType::Standard | GamepadType::Unknown => "Xbox-compatible",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PadEvent {
    Press { id: u32, pad: Pad },
    Connected { id: u32, name: String, layout: Layout },
    Disconnected { id: u32 },
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
        Button::LeftShoulder => Pad::Lb,
        Button::RightShoulder => Pad::Rb,
        Button::Start => Pad::Start,
        Button::Back => Pad::Select,
        _ => return None,
    })
}

const FIRST_REPEAT: Duration = Duration::from_millis(320);
const NEXT_REPEAT: Duration = Duration::from_millis(90);

/// Auto-repeat for a held direction.
#[derive(Debug, Default)]
struct Repeater {
    held: Option<(u32, Pad, Instant)>,
}

impl Repeater {
    fn press(&mut self, id: u32, pad: Pad, now: Instant) {
        self.held = Some((id, pad, now + FIRST_REPEAT));
    }

    fn release(&mut self, id: u32, pad: Pad) {
        if self.held.is_some_and(|(i, p, _)| i == id && p == pad) {
            self.held = None;
        }
    }

    fn release_all(&mut self, id: u32) {
        if self.held.is_some_and(|(i, _, _)| i == id) {
            self.held = None;
        }
    }

    /// The press to repeat now, if one is due.
    fn due(&mut self, now: Instant) -> Option<(u32, Pad)> {
        let (id, pad, next) = self.held.as_mut()?;
        if now >= *next {
            *next = now + NEXT_REPEAT;
            Some((*id, *pad))
        } else {
            None
        }
    }

    fn wait(&self, now: Instant) -> Option<Duration> {
        self.held.map(|(_, _, next)| next.saturating_duration_since(now))
    }
}

/// Turns a stick into direction presses, with hysteresis so a stick
/// resting near the threshold doesn't flicker.
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

fn silent_log(_: sdl3::log::Priority, _: sdl3::log::Category, _: &str) {}

/// Start reading controllers. Returns quietly if SDL can't be initialised.
pub fn spawn(tx: Sender<PadEvent>) {
    thread::spawn(move || {
        let _ = run(tx);
    });
}

fn run(tx: Sender<PadEvent>) -> Result<(), String> {
    // SDL would otherwise log to stderr, on top of the TUI.
    sdl3::log::set_output_function(silent_log);
    // There's no SDL window to have focus; the TUI tracks focus itself.
    sdl3::hint::set("SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS", "1");
    let sdl = sdl3::init().map_err(|e| e.to_string())?;
    let subsystem = sdl.gamepad().map_err(|e| e.to_string())?;
    let mut events = sdl.event_pump().map_err(|e| e.to_string())?;

    // Controllers have to stay open to deliver events. SDL also reports
    // the ones connected at startup as added.
    let mut open: HashMap<u32, Gamepad> = HashMap::new();
    let mut sticks: HashMap<u32, Stick> = HashMap::new();
    let mut repeat = Repeater::default();
    loop {
        let wait = repeat.wait(Instant::now()).unwrap_or(Duration::from_millis(500));
        let mut out = Vec::new();
        if let Some(event) = events.wait_event_timeout(wait) {
            let now = Instant::now();
            match event {
                Event::GamepadAdded { which, .. } => {
                    let id = which.raw();
                    if let Ok(pad) = subsystem.open(which) {
                        let name = pad.name().unwrap_or_else(|| "Controller".into());
                        out.push(PadEvent::Connected { id, name, layout: Layout::from_sdl(&pad) });
                        open.insert(id, pad);
                    }
                }
                Event::GamepadRemoved { which, .. } => {
                    let id = which.raw();
                    open.remove(&id);
                    sticks.remove(&id);
                    repeat.release_all(id);
                    out.push(PadEvent::Disconnected { id });
                }
                Event::GamepadButtonDown { which, button, .. } => {
                    let id = which.raw();
                    if let Some(pad) = map_button(button) {
                        out.push(PadEvent::Press { id, pad });
                        if pad.is_direction() {
                            repeat.press(id, pad, now);
                        }
                    }
                }
                Event::GamepadButtonUp { which, button, .. } => {
                    if let Some(pad) = map_button(button) {
                        repeat.release(which.raw(), pad);
                    }
                }
                Event::GamepadAxisMotion { which, axis, value, .. } => {
                    let id = which.raw();
                    let v = value as f32 / 32767.0;
                    // SDL's y axis points down.
                    let (x, y) = match axis {
                        Axis::LeftX => (Some(v), None),
                        Axis::LeftY => (None, Some(-v)),
                        _ => (None, None),
                    };
                    if x.is_some() || y.is_some() {
                        let (released, pressed) = sticks.entry(id).or_default().update(x, y);
                        if let Some(r) = released {
                            repeat.release(id, r);
                        }
                        if let Some(pad) = pressed {
                            out.push(PadEvent::Press { id, pad });
                            repeat.press(id, pad, now);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some((id, pad)) = repeat.due(Instant::now()) {
            out.push(PadEvent::Press { id, pad });
        }
        for e in out {
            if tx.send(e).is_err() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(family: Family, faces: [Face; 4]) -> Layout {
        Layout { family, faces, kind: "test" }
    }

    #[test]
    fn labels_come_from_the_faces() {
        let xbox = Layout::default();
        assert_eq!(xbox.label(Pad::South), ("A", Some(Color::Rgb(130, 200, 100))));
        assert_eq!(xbox.label(Pad::Lb).0, "LB");

        let ps = layout(Family::PlayStation, [Face::Cross, Face::Circle, Face::Square, Face::Triangle]);
        assert_eq!(ps.label(Pad::South).0, "✕");
        assert_eq!(ps.label(Pad::North).0, "△");
        assert_eq!(ps.label(Pad::Rb).0, "R1");

        // SDL reports Nintendo's own letters by position: B at the bottom.
        let switch = layout(Family::Nintendo, [Face::B, Face::A, Face::Y, Face::X]);
        assert_eq!(switch.label(Pad::South), ("B", None));
        assert_eq!(switch.label(Pad::East).0, "A");
        assert_eq!(switch.label(Pad::Start).0, "+");
    }

    #[test]
    fn repeats_held_directions() {
        let t0 = Instant::now();
        let mut r = Repeater::default();
        assert_eq!(r.due(t0), None);
        r.press(1, Pad::Down, t0);
        assert_eq!(r.due(t0 + Duration::from_millis(100)), None, "initial delay");
        assert_eq!(r.due(t0 + FIRST_REPEAT), Some((1, Pad::Down)));
        assert_eq!(r.due(t0 + FIRST_REPEAT + Duration::from_millis(10)), None);
        assert_eq!(r.due(t0 + FIRST_REPEAT + NEXT_REPEAT), Some((1, Pad::Down)));
        r.release(1, Pad::Up);
        r.release(2, Pad::Down);
        assert!(r.held.is_some(), "only the same button on the same pad stops it");
        r.release(1, Pad::Down);
        assert_eq!(r.due(t0 + Duration::from_secs(5)), None);
        r.press(3, Pad::Left, t0);
        r.release_all(3);
        assert_eq!(r.due(t0 + Duration::from_secs(5)), None, "unplugging stops the repeat");
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
        assert_eq!(s.update(None, Some(1.0)), (None, Some(Pad::Up)));
    }
}
