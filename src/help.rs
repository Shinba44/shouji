//! ヘルプ画面（C-q ? かステータスバーの ? Help で開く）．キー操作とマウス操作の一覧．

use crossterm::style::Color;
use unicode_width::UnicodeWidthStr;

use crate::launcher::put_clipped;
use crate::render::{Frame, Style};

enum Line {
    Text(&'static str),
    Section(&'static str),
    Key(&'static str, &'static str),
    Blank,
}

use Line::*;

const LINES: &[Line] = &[
    Text("All shortcuts start with Ctrl-q (written C-q):"),
    Text("press Ctrl-q, let go, then press the key."),
    Blank,
    Section("Start programs"),
    Key("C-q Space", "Open the task launcher"),
    Key("", "Enter: start everything   ->: choose what to start"),
    Blank,
    Section("Panes"),
    Key("C-q %", "Split left / right"),
    Key("C-q \"", "Split top / bottom"),
    Key("C-q o", "Move to the next pane"),
    Key("C-q arrows, h/j/k/l", "Move to the pane on that side"),
    Key("C-q x", "Close the pane"),
    Blank,
    Section("Windows"),
    Key("C-q c", "New window"),
    Key("C-q n / C-q p", "Next / previous window"),
    Key("C-q 0-9", "Go to window number N"),
    Blank,
    Section("Reading past output (scroll mode)"),
    Key("C-q [", "Start scrolling (or use the mouse wheel)"),
    Key("k / j, arrows", "One line up / down"),
    Key("u / d, PgUp / PgDn", "Half / full page up / down"),
    Key("g / G", "Oldest / newest output"),
    Key("q, Esc", "Stop scrolling"),
    Blank,
    Section("When a program has stopped (keys in its pane)"),
    Key("r", "Run it again"),
    Key("s", "Open a shell in its directory"),
    Key("q", "Close the pane"),
    Blank,
    Section("When a program is waiting to start"),
    Key("Enter", "Start it now"),
    Key("q", "Close the pane"),
    Blank,
    Section("Mouse"),
    Key("Click", "Select a pane, press a button"),
    Key("Wheel", "Scroll through past output"),
    Key("Shift + drag", "Select text to copy"),
    Key("C-q m", "Turn the mouse on / off"),
    Blank,
    Section("Other"),
    Key("C-q d", "Quit shouji"),
    Key("C-q C-q", "Send Ctrl-q to the program"),
    Key("C-q ?", "Show this help"),
];

const KEY_WIDTH: usize = 22;
const TITLE: &str = " Help ";

/// ヘルプ画面．`offset` は表示している先頭の行．
#[derive(Default)]
pub struct Help {
    offset: usize,
}

struct Geometry {
    x0: u16,
    y0: u16,
    w: u16,
    h: u16,
}

impl Geometry {
    fn visible(&self) -> usize {
        self.h.saturating_sub(2) as usize
    }

    fn contains(&self, x: u16, y: u16) -> bool {
        (self.x0..self.x0 + self.w).contains(&x) && (self.y0..self.y0 + self.h).contains(&y)
    }
}

fn line_width(l: &Line) -> usize {
    match l {
        Text(t) | Section(t) => t.width() + 1,
        Key(_, d) => 2 + KEY_WIDTH + d.width() + 1,
        Blank => 0,
    }
}

impl Help {
    /// キー入力を処理する．閉じるなら true．
    pub fn feed(&mut self, bytes: &[u8], fw: u16, fh: u16) -> bool {
        let page = geometry(fw, fh).map_or(1, |g| g.visible().max(1));
        if bytes == b"\x1b" {
            return true;
        }
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            let (step, delta): (usize, isize) = match rest {
                [0x1b, b'[', b'5', b'~', ..] => (4, -(page as isize)),
                [0x1b, b'[', b'6', b'~', ..] => (4, page as isize),
                [0x1b, b'[' | b'O', b'A', ..] => (3, -1),
                [0x1b, b'[' | b'O', b'B', ..] => (3, 1),
                [0x1b, b'[' | b'O', _, ..] => (3, 0),
                [b'q' | b'?' | b'\r' | 0x1b, ..] => return true,
                [b'k', ..] => (1, -1),
                [b'j', ..] => (1, 1),
                [b'u', ..] => (1, -(page as isize)),
                [b'd' | b' ', ..] => (1, page as isize),
                [b'g', ..] => (1, isize::MIN / 2),
                [b'G', ..] => (1, isize::MAX / 2),
                _ => (1, 0),
            };
            self.scroll(delta, fw, fh);
            i += step;
        }
        false
    }

    pub fn scroll(&mut self, delta: isize, fw: u16, fh: u16) {
        let max = geometry(fw, fh).map_or(0, |g| LINES.len().saturating_sub(g.visible()));
        self.offset = (self.offset as isize).saturating_add(delta).clamp(0, max as isize) as usize;
    }

    /// クリックした位置がヘルプの外なら true（閉じる）．
    pub fn click_outside(&self, x: u16, y: u16, fw: u16, fh: u16) -> bool {
        geometry(fw, fh).is_none_or(|g| !g.contains(x, y))
    }

    /// 画面中央に枠付きで描く．`fh` はステータスバーを含む端末の高さ．
    pub fn draw(&self, frame: &mut Frame) {
        let Some(g) = geometry(frame.w, frame.h) else { return };
        let Geometry { x0, y0, w, h } = g;
        let border = Style { fg: Color::Cyan, ..Style::default() };
        let normal = Style::default();
        let section = Style { fg: Color::Yellow, bold: true, ..Style::default() };
        let key = Style { fg: Color::Cyan, bold: true, ..Style::default() };

        for y in y0..y0 + h {
            for x in x0..x0 + w {
                let ch = match (x == x0, x == x0 + w - 1, y == y0, y == y0 + h - 1) {
                    (true, _, true, _) => "┌",
                    (_, true, true, _) => "┐",
                    (true, _, _, true) => "└",
                    (_, true, _, true) => "┘",
                    (_, _, true, _) | (_, _, _, true) => "─",
                    (true, _, _, _) | (_, true, _, _) => "│",
                    _ => " ",
                };
                frame.set(x, y, ch, if ch == " " { normal } else { border });
            }
        }
        let end = x0 + w - 1;
        put_clipped(frame, x0 + 2, y0, TITLE, Style { bold: true, ..border }, end);
        let visible = g.visible();
        let more = LINES.len() > visible;
        let footer = if more {
            format!(" q: close  j/k: scroll  ({}-{} of {}) ", self.offset + 1, (self.offset + visible).min(LINES.len()), LINES.len())
        } else {
            " q: close ".to_string()
        };
        put_clipped(frame, x0 + 2, y0 + h - 1, &footer, border, end);

        for (i, line) in LINES.iter().skip(self.offset).take(visible).enumerate() {
            let y = y0 + 1 + i as u16;
            let x = x0 + 2;
            match line {
                Text(t) => {
                    put_clipped(frame, x, y, t, normal, end);
                }
                Section(t) => {
                    put_clipped(frame, x, y, t, section, end);
                }
                Key(k, d) => {
                    put_clipped(frame, x + 2, y, k, key, end);
                    put_clipped(frame, x + 2 + KEY_WIDTH as u16, y, d, normal, end);
                }
                Blank => {}
            }
        }
    }
}

/// 枠の位置と大きさ．ステータスバー（最下行）にはかからないようにする．
fn geometry(fw: u16, fh: u16) -> Option<Geometry> {
    let area_h = fh.saturating_sub(1);
    let inner_w = LINES.iter().map(line_width).max().unwrap_or(0) as u16 + 2;
    let w = (inner_w + 2).min(fw);
    let h = (LINES.len() as u16 + 2).min(area_h);
    if w < 10 || h < 3 {
        return None;
    }
    Some(Geometry { x0: (fw - w) / 2, y0: (area_h - h) / 2, w, h })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_and_close() {
        let mut h = Help::default();
        // 背が低い端末ではスクロールできる
        assert!(!h.feed(b"jj", 80, 12));
        assert_eq!(h.offset, 2);
        assert!(!h.feed(b"G", 80, 12));
        assert_eq!(h.offset, LINES.len() - 9);
        assert!(!h.feed(b"g", 80, 12));
        assert_eq!(h.offset, 0);
        // 全部入るならスクロールしない
        assert!(!h.feed(b"j", 80, 100));
        assert_eq!(h.offset, 0);
        assert!(h.feed(b"q", 80, 12));
        assert!(h.feed(b"\x1b", 80, 12));
        assert!(h.click_outside(0, 0, 200, 12));
        assert!(!h.click_outside(100, 5, 200, 12));
    }
}
