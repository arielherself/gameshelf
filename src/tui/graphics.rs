//! Artwork in the TUI via the kitty graphics protocol.
//!
//! During a frame the UI asks for artwork in cell rectangles ([`Graphics::slot`]);
//! after ratatui has drawn the text, [`Graphics::sync`] makes the terminal's
//! placements match: new images are transmitted once and placed, moved or
//! hidden ones are removed. A worker thread downloads, decodes and scales
//! images to the exact pixel size of their rectangle, newest request first.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use image::DynamicImage;
use ratatui::layout::Rect;
use ratatui::style::Color;

use super::kitty::{self, CellPx};
use crate::art::{self, ArtKind};

/// An image at a specific size in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Spec {
    pub appid: u32,
    pub kind: ArtKind,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Ready,
    Loading,
    /// The app has no such artwork (or it failed to load).
    Missing,
}

struct Request {
    spec: Spec,
    px: (u32, u32),
}

enum Outcome {
    Ready(kitty::Encoded),
    Missing,
}

struct Response {
    spec: Spec,
    outcome: Outcome,
    accent: Option<(u8, u8, u8)>,
}

struct Prepared {
    id: u32,
    /// Kept so the image can be sent again: terminals may drop image data
    /// when the screen is cleared (kitty does on resize) or under memory
    /// pressure, and a placement of a dropped image shows nothing.
    data: kitty::Encoded,
    /// Whether the terminal currently holds `data` under `id`.
    transmitted: bool,
    last_used: u64,
}

type Queue = Arc<(Mutex<Vec<Request>>, Condvar)>;

pub struct Graphics {
    enabled: bool,
    cell: CellPx,
    queue: Queue,
    responses: Receiver<Response>,
    prepared: HashMap<Spec, Prepared>,
    requested: HashSet<Spec>,
    missing: HashSet<(u32, ArtKind)>,
    accents: HashMap<u32, (Color, ArtKind)>,
    /// Currently displayed: spec → top-left cell.
    placed: HashMap<Spec, (u16, u16)>,
    /// Requested by the frame being drawn.
    slots: Vec<(Spec, Rect)>,
    next_id: u32,
    frame: u64,
}

/// Prepared images kept around (in the terminal and in memory) for reuse.
const KEEP_PREPARED: usize = 24;
const WORKERS: usize = 3;
/// Originals are kept in memory at most this large (for re-scaling).
const MAX_ORIGINAL: (u32, u32) = (1920, 1080);

impl Graphics {
    /// Detect protocol support (`GAMESHELF_GRAPHICS=kitty|none` overrides).
    /// Call right after entering raw mode.
    pub fn init(offline: bool) -> Self {
        let mode = std::env::var("GAMESHELF_GRAPHICS").unwrap_or_default().to_lowercase();
        let (enabled, cell) = match mode.as_str() {
            "none" | "off" | "0" | "false" => (false, None),
            "kitty" | "on" | "1" | "true" => (true, None),
            _ => {
                let d = kitty::detect(std::time::Duration::from_millis(400));
                (d.supported, d.cell)
            }
        };
        let cell = cell.or_else(kitty::cell_from_winsize).unwrap_or_default();

        let queue: Queue = Arc::default();
        let (tx, responses) = channel();
        if enabled {
            // Mostly waiting on the network, so a few in parallel.
            for _ in 0..WORKERS {
                let (q, tx) = (queue.clone(), tx.clone());
                thread::spawn(move || worker(q, tx, offline));
            }
        }
        // Ids are global to the terminal window; start somewhere unlikely to
        // collide with other programs.
        let next_id = ((std::process::id() & 0x3fff) << 16) | 1;
        Self {
            enabled,
            cell,
            queue,
            responses,
            prepared: HashMap::new(),
            requested: HashSet::new(),
            missing: HashSet::new(),
            accents: HashMap::new(),
            placed: HashMap::new(),
            slots: Vec::new(),
            next_id,
            frame: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Height / width of a cell; terminals' cells are usually about 2.
    pub fn cell_ratio(&self) -> f64 {
        self.cell.h / self.cell.w
    }

    pub fn accent(&self, appid: u32) -> Option<Color> {
        self.accents.get(&appid).map(|(c, _)| *c)
    }

    /// Largest rectangle inside `area` with the artwork's aspect ratio,
    /// centred horizontally and aligned to the top.
    pub fn fit(&self, kind: ArtKind, area: Rect) -> Rect {
        fit(kind.aspect(), self.cell_ratio(), area)
    }

    /// Request artwork for exactly `rect` in this frame.
    pub fn slot(&mut self, appid: u32, kind: ArtKind, rect: Rect) -> SlotState {
        if !self.enabled || rect.width == 0 || rect.height == 0 {
            return SlotState::Missing;
        }
        if self.missing.contains(&(appid, kind)) {
            return SlotState::Missing;
        }
        let spec = Spec { appid, kind, cols: rect.width, rows: rect.height };
        self.slots.push((spec, rect));
        if self.prepared.contains_key(&spec) { SlotState::Ready } else { SlotState::Loading }
    }

    /// Whether any artwork is still on its way (for redraw pacing).
    pub fn busy(&self) -> bool {
        !self.requested.is_empty()
    }

    /// Bring the terminal in line with this frame's slots. Slots overlapping
    /// `overlay` (an open popup) are hidden, since images draw over text.
    pub fn sync(&mut self, out: &mut impl Write, overlay: Option<Rect>) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.frame += 1;
        self.receive();

        let slots = std::mem::take(&mut self.slots);
        self.drop_unwanted_requests(&slots);
        let mut buf = String::new();
        let desired: HashMap<Spec, Rect> = slots
            .iter()
            .filter(|(_, r)| overlay.is_none_or(|o| !o.intersects(*r)))
            .map(|(s, r)| (*s, *r))
            .collect();

        // Remove placements that moved or are no longer wanted.
        let stale: Vec<Spec> = self
            .placed
            .iter()
            .filter(|(spec, pos)| desired.get(spec).is_none_or(|r| (r.x, r.y) != **pos))
            .map(|(spec, _)| *spec)
            .collect();
        for spec in stale {
            self.placed.remove(&spec);
            if let Some(p) = self.prepared.get(&spec) {
                buf.push_str(&kitty::unplace(p.id));
            }
        }

        for (spec, rect) in &desired {
            match self.prepared.get_mut(spec) {
                Some(p) => {
                    p.last_used = self.frame;
                    if self.placed.contains_key(spec) {
                        continue;
                    }
                    if !p.transmitted {
                        buf.push_str(&kitty::transmit(p.id, &p.data));
                        p.transmitted = true;
                    }
                    buf.push_str(&kitty::place(p.id, rect.x, rect.y, rect.width, rect.height));
                    self.placed.insert(*spec, (rect.x, rect.y));
                }
                None => self.request(*spec),
            }
        }
        // Also keep images for hidden slots (e.g. under a popup) warm.
        for (spec, _) in &slots {
            if let Some(p) = self.prepared.get_mut(spec) {
                p.last_used = self.frame;
            }
        }
        self.evict(&mut buf);

        if !buf.is_empty() {
            out.write_all(buf.as_bytes())?;
            out.flush()?;
        }
        Ok(())
    }

    /// The screen was cleared (e.g. on resize): forget what's displayed.
    pub fn invalidate(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        if let Some(c) = kitty::cell_from_winsize() {
            self.cell = c;
        }
        // Free rather than unplace: after a clear we can't know what the
        // terminal kept, so everything is sent again when next shown.
        let mut buf = String::new();
        for p in self.prepared.values_mut() {
            if p.transmitted {
                buf.push_str(&kitty::free(p.id));
                p.transmitted = false;
            }
        }
        self.placed.clear();
        out.write_all(buf.as_bytes())?;
        out.flush()
    }

    /// Free everything we sent to the terminal.
    pub fn shutdown(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let buf: String = self.prepared.values().map(|p| kitty::free(p.id)).collect();
        self.prepared.clear();
        self.placed.clear();
        out.write_all(buf.as_bytes())?;
        out.flush()
    }

    /// Forget queued work for artwork no longer on screen (e.g. after
    /// scrolling past it), so what's visible now loads first.
    fn drop_unwanted_requests(&mut self, slots: &[(Spec, Rect)]) {
        let wanted: HashSet<Spec> = slots.iter().map(|(s, _)| *s).collect();
        let (lock, _) = &*self.queue;
        let mut q = lock.lock().unwrap();
        q.retain(|r| {
            let keep = wanted.contains(&r.spec);
            if !keep {
                self.requested.remove(&r.spec);
            }
            keep
        });
    }

    fn request(&mut self, spec: Spec) {
        if !self.requested.insert(spec) {
            return;
        }
        let px = (
            (spec.cols as f64 * self.cell.w).round() as u32,
            (spec.rows as f64 * self.cell.h).round() as u32,
        );
        let (lock, cvar) = &*self.queue;
        lock.lock().unwrap().push(Request { spec, px });
        cvar.notify_one();
    }

    fn receive(&mut self) {
        while let Ok(r) = self.responses.try_recv() {
            self.requested.remove(&r.spec);
            if let Some((cr, cg, cb)) = r.accent {
                // The cover decides the accent; the hero only fills in.
                let keep = self.accents.get(&r.spec.appid).is_some_and(|(_, k)| *k == ArtKind::Cover);
                if !keep {
                    self.accents.insert(r.spec.appid, (Color::Rgb(cr, cg, cb), r.spec.kind));
                }
            }
            match r.outcome {
                Outcome::Ready(data) => {
                    let id = self.next_id;
                    self.next_id = self.next_id.wrapping_add(1).max(1);
                    self.prepared
                        .insert(r.spec, Prepared { id, data, transmitted: false, last_used: self.frame });
                }
                Outcome::Missing => {
                    self.missing.insert((r.spec.appid, r.spec.kind));
                }
            }
        }
    }

    fn evict(&mut self, buf: &mut String) {
        if self.prepared.len() <= KEEP_PREPARED {
            return;
        }
        let mut idle: Vec<(u64, Spec)> = self
            .prepared
            .iter()
            .filter(|(s, _)| !self.placed.contains_key(s))
            .map(|(s, p)| (p.last_used, *s))
            .collect();
        idle.sort_unstable_by_key(|(used, _)| *used);
        let excess = self.prepared.len() - KEEP_PREPARED;
        for (_, spec) in idle.into_iter().take(excess) {
            if let Some(p) = self.prepared.remove(&spec)
                && p.transmitted
            {
                buf.push_str(&kitty::free(p.id));
            }
        }
    }
}

/// See [`Graphics::fit`]. `aspect` is image width / height in pixels,
/// `cell_ratio` is cell height / width.
pub fn fit(aspect: f64, cell_ratio: f64, area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect { width: 0, height: 0, ..area };
    }
    // Width in cells that `rows` rows of the image would need.
    let cols_for = |rows: f64| rows * cell_ratio * aspect;
    let (cols, rows) = if cols_for(area.height as f64) <= area.width as f64 {
        (cols_for(area.height as f64).round().max(1.0) as u16, area.height)
    } else {
        let rows = (area.width as f64 / (cell_ratio * aspect)).round().max(1.0) as u16;
        (area.width, rows.min(area.height))
    };
    let cols = cols.min(area.width);
    Rect { x: area.x + (area.width - cols) / 2, y: area.y, width: cols, height: rows }
}

fn worker(queue: Queue, tx: Sender<Response>, offline: bool) {
    // Recently decoded originals, so a resize doesn't decode again.
    let mut decoded: VecDeque<((u32, ArtKind), Option<DynamicImage>)> = VecDeque::new();
    loop {
        let req = {
            let (lock, cvar) = &*queue;
            let mut q = lock.lock().unwrap();
            loop {
                if let Some(r) = q.pop() {
                    break r;
                }
                q = cvar.wait(q).unwrap();
            }
        };
        let key = (req.spec.appid, req.spec.kind);
        let img = match decoded.iter().position(|(k, _)| *k == key) {
            Some(i) => decoded[i].1.clone(),
            None => {
                let img = art::load(key.0, key.1, offline).ok().flatten().map(|img| {
                    if img.width() > MAX_ORIGINAL.0 || img.height() > MAX_ORIGINAL.1 {
                        img.resize(MAX_ORIGINAL.0, MAX_ORIGINAL.1, image::imageops::FilterType::Triangle)
                    } else {
                        img
                    }
                });
                decoded.push_back((key, img.clone()));
                if decoded.len() > 8 {
                    decoded.pop_front();
                }
                img
            }
        };
        let response = match img {
            Some(img) => {
                let (w, h) = (req.px.0.max(1), req.px.1.max(1));
                let scaled = img.resize_to_fill(w, h, image::imageops::FilterType::Triangle).to_rgb8();
                Response {
                    spec: req.spec,
                    accent: Some(art::accent(&img)),
                    outcome: Outcome::Ready(kitty::encode(w, h, scaled.as_raw())),
                }
            }
            None => Response { spec: req.spec, outcome: Outcome::Missing, accent: None },
        };
        if tx.send(response).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_covers_and_heroes() {
        let area = Rect::new(10, 5, 40, 20);
        // 2:3 cover with 2:1 cells: 20 rows → 20*2*(2/3) ≈ 27 cols.
        let r = fit(600.0 / 900.0, 2.0, area);
        assert_eq!((r.width, r.height), (27, 20));
        assert_eq!(r.x, 10 + (40 - 27) / 2);
        assert_eq!(r.y, 5);
        // A wide hero is limited by width instead.
        let r = fit(3840.0 / 1240.0, 2.0, area);
        assert_eq!(r.width, 40);
        assert_eq!(r.height, 6);
        assert_eq!(fit(1.0, 2.0, Rect::new(0, 0, 0, 5)).width, 0);
    }
}
