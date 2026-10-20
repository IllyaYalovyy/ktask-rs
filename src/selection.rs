use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::Rect,
    style::{Color, Style},
};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Point {
    x: u16,
    y: u16,
}

fn ordered(a: Point, b: Point) -> (Point, Point) {
    if (a.y, a.x) <= (b.y, b.x) {
        (a, b)
    } else {
        (b, a)
    }
}

#[derive(Default)]
pub struct Selection {
    zones: Vec<Rect>,
    screen: Vec<Vec<Option<String>>>,
    down: Option<Point>,
    anchor: Option<Point>,
    end: Option<Point>,
    zone: Option<usize>,
    keyboard: bool,
    requested: bool,
    frozen: Option<String>,
}

impl Selection {
    pub fn begin_frame(&mut self) {
        self.zones.clear();
    }
    pub fn zone(&mut self, rect: Rect) {
        if rect.width > 0 && rect.height > 0 {
            self.zones.push(rect);
        }
    }
    fn at(&self, p: Point) -> Option<usize> {
        self.zones
            .iter()
            .position(|r| r.contains((p.x, p.y).into()))
    }
    fn clamp(&self, p: Point, zone: usize) -> Point {
        let r = self.zones[zone];
        Point {
            x: p.x.clamp(r.x, r.right() - 1),
            y: p.y.clamp(r.y, r.bottom() - 1),
        }
    }
    pub fn clear(&mut self) {
        self.down = None;
        self.anchor = None;
        self.end = None;
        self.zone = None;
        self.keyboard = false;
        self.frozen = None;
    }
    pub fn active(&self) -> bool {
        self.anchor.is_some() || self.keyboard
    }
    pub fn mouse_down(&mut self, x: u16, y: u16) {
        self.clear();
        self.down = Some(Point { x, y });
    }
    pub fn mouse_drag(&mut self, x: u16, y: u16) {
        let Some(start) = self.down else { return };
        let Some(zone) = self.at(start) else { return };
        if self.anchor.is_none() {
            self.anchor = Some(start);
            self.zone = Some(zone);
        }
        self.end = Some(self.clamp(Point { x, y }, zone));
        self.keyboard = false;
    }
    pub fn keyboard_in(&self, rect: Rect) -> bool {
        self.keyboard && self.end.is_some_and(|p| rect.contains((p.x, p.y).into()))
    }
    pub fn mouse_up(&mut self, x: u16, y: u16) -> bool {
        let down = self.down.take();
        if let Some(zone) = self.zone.filter(|_| self.anchor.is_some()) {
            self.end = Some(self.clamp(Point { x, y }, zone));
            self.frozen = self.selected_text();
            return true;
        }
        self.clear();
        down != Some(Point { x, y })
    }
    pub fn keyboard_key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::F(2) {
            if self.zones.is_empty() {
                return true;
            }
            self.frozen = None;
            let zone = if self.keyboard {
                (self.zone.unwrap_or(0) + 1) % self.zones.len()
            } else {
                0
            };
            let r = self.zones[zone];
            let point = Point { x: r.x, y: r.y };
            self.zone = Some(zone);
            self.anchor = None;
            self.end = Some(point);
            self.keyboard = true;
            return true;
        }
        if !self.keyboard {
            return false;
        }
        let Some(zone) = self.zone else { return false };
        let r = self.zones[zone];
        let p = self.end.unwrap_or(Point { x: r.x, y: r.y });
        let next = match key.code {
            KeyCode::Left => Point {
                x: p.x.saturating_sub(1).max(r.x),
                ..p
            },
            KeyCode::Right => Point {
                x: (p.x + 1).min(r.right() - 1),
                ..p
            },
            KeyCode::Up => Point {
                y: p.y.saturating_sub(1).max(r.y),
                ..p
            },
            KeyCode::Down => Point {
                y: (p.y + 1).min(r.bottom() - 1),
                ..p
            },
            KeyCode::Home => Point { x: r.x, ..p },
            KeyCode::End => Point {
                x: r.right() - 1,
                ..p
            },
            KeyCode::PageUp => Point {
                y: p.y.saturating_sub(r.height).max(r.y),
                ..p
            },
            KeyCode::PageDown => Point {
                y: p.y.saturating_add(r.height).min(r.bottom() - 1),
                ..p
            },
            KeyCode::Enter => {
                self.requested = true;
                return true;
            }
            KeyCode::Esc => {
                self.clear();
                return true;
            }
            _ => {
                self.clear();
                return false;
            }
        };
        self.frozen = None;
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            self.anchor.get_or_insert(p);
        } else {
            self.anchor = None;
        }
        self.end = Some(next);
        true
    }
    pub fn copy_requested(&mut self) -> bool {
        std::mem::take(&mut self.requested)
    }
    pub fn finish_frame(&mut self, buffer: &mut Buffer) {
        let area = buffer.area;
        self.screen = vec![vec![None; area.width as usize]; area.height as usize];
        for zone in &self.zones {
            for y in zone.y..zone.bottom().min(area.bottom()) {
                for x in zone.x..zone.right().min(area.right()) {
                    if let Some(cell) = buffer.cell((x, y)) {
                        self.screen[(y - area.y) as usize][(x - area.x) as usize] =
                            Some(cell.symbol().to_owned());
                    }
                }
            }
        }
        let Some(end) = self.end else { return };
        let Some(zone) = self.zone.filter(|i| *i < self.zones.len()) else {
            self.clear();
            return;
        };
        if !self.zones[zone].contains((end.x, end.y).into()) {
            self.clear();
            return;
        }
        let (first, last) = self.anchor.map_or((end, end), |a| ordered(a, end));
        for y in first.y..=last.y {
            let left = if y == first.y {
                first.x
            } else {
                self.zones[zone].x
            };
            let right = if y == last.y {
                last.x
            } else {
                self.zones[zone].right() - 1
            };
            for x in left..=right {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_style(Style::new().fg(Color::Black).bg(Color::Rgb(255, 210, 111)));
                }
            }
        }
    }
    pub fn text(&self) -> Option<String> {
        self.frozen.clone().or_else(|| self.selected_text())
    }
    fn selected_text(&self) -> Option<String> {
        let (a, b) = (self.anchor?, self.end?);
        if a == b {
            return None;
        }
        let zone = *self.zones.get(self.zone?)?;
        let (first, last) = ordered(a, b);
        let mut lines = Vec::new();
        for y in first.y..=last.y {
            let left = if y == first.y { first.x } else { zone.x };
            let right = if y == last.y {
                last.x
            } else {
                zone.right() - 1
            };
            let mut line = String::new();
            for x in left..=right {
                if let Some(s) = self
                    .screen
                    .get(y as usize)
                    .and_then(|row| row.get(x as usize))
                    .and_then(Option::as_ref)
                {
                    line.push_str(s);
                }
            }
            lines.push(line.trim_end().to_owned());
        }
        Some(lines.join("\n").trim_end().to_owned())
    }
}

pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
