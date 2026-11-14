//! shouji.yaml のタスク一覧から選んで起動するメニュー．

use crossterm::style::Color;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::TaskEntry;
use crate::render::{Frame, Style};

/// メニューでの操作結果．
pub enum MenuResult {
    /// メニューを開いたままにする．
    Stay,
    Cancel,
    /// 選んだタスク（添字）を起動する．
    Launch(usize),
}

const TITLE: &str = " Launch task ";
const FOOTER: &str = " Enter/click:launch  q:cancel ";

struct Geometry {
    x0: u16,
    y0: u16,
    w: u16,
    h: u16,
    lines: Vec<String>,
}

#[derive(Default)]
pub struct Launcher {
    pub selected: usize,
}

impl Launcher {
    /// キー入力（生バイト列）を処理する．
    pub fn feed(&mut self, bytes: &[u8], len: usize) -> MenuResult {
        if len == 0 {
            return MenuResult::Cancel;
        }
        // 単独の ESC はキャンセル，ESC [ A などは矢印キー
        if bytes == b"\x1b" {
            return MenuResult::Cancel;
        }
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            let (step, key) = match rest {
                [0x1b, b'[' | b'O', k, ..] => (3, Some(*k as char).filter(|c| "AB".contains(*c)).map(|c| if c == 'A' { 'k' } else { 'j' })),
                [b, ..] => (1, Some(*b as char)),
                [] => break,
            };
            i += step;
            match key {
                Some('k') | Some('\x10') => self.selected = (self.selected + len - 1) % len, // ↑ / k / C-p
                Some('j') | Some('\x0e') => self.selected = (self.selected + 1) % len,       // ↓ / j / C-n
                Some('\r') | Some('\n') => return MenuResult::Launch(self.selected),
                Some('q') | Some('\x1b') | Some('\x11') => return MenuResult::Cancel,
                Some(c @ '1'..='9') => {
                    let n = c as usize - '1' as usize;
                    if n < len {
                        return MenuResult::Launch(n);
                    }
                }
                _ => {}
            }
        }
        MenuResult::Stay
    }

    /// メニューの位置と各行の文字列を求める（描画とクリック判定で共通）．
    fn geometry(&self, fw: u16, fh: u16, tasks: &[TaskEntry]) -> Option<Geometry> {
        let lines: Vec<String> = if tasks.is_empty() {
            vec!["(no tasks: write tasks: in shouji.yaml)".into()]
        } else {
            tasks
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let key = if i < 9 { format!("{}", i + 1) } else { " ".into() };
                    format!(" {key}  {:<12} {}", t.name, t.path.display())
                })
                .collect()
        };
        let width = |s: &str| s.width() as u16;
        let inner_w = lines.iter().map(|l| width(l) + 1).chain([width(FOOTER), width(TITLE)]).max().unwrap_or(0);
        let w = (inner_w + 2).min(fw);
        let h = (lines.len() as u16 + 2).min(fh.saturating_sub(1));
        if w < 4 || h < 3 {
            return None;
        }
        let x0 = (fw - w) / 2;
        let y0 = (fh.saturating_sub(1) - h) / 2;
        Some(Geometry { x0, y0, w, h, lines })
    }

    /// 画面の (x, y) がクリックされたときの結果．項目ならその起動，
    /// メニューの外ならキャンセル．
    pub fn click(&mut self, x: u16, y: u16, fw: u16, fh: u16, tasks: &[TaskEntry]) -> MenuResult {
        let Some(g) = self.geometry(fw, fh, tasks) else { return MenuResult::Cancel };
        let inside = x >= g.x0 && x < g.x0 + g.w && y >= g.y0 && y < g.y0 + g.h;
        if !inside {
            return MenuResult::Cancel;
        }
        let row = (y as usize).wrapping_sub(g.y0 as usize + 1);
        if row < tasks.len() && y < g.y0 + g.h - 1 && x > g.x0 && x < g.x0 + g.w - 1 {
            self.selected = row;
            return MenuResult::Launch(row);
        }
        MenuResult::Stay
    }

    /// ホイールで選択を上下させる．
    pub fn wheel(&mut self, up: bool, len: usize) {
        if len > 0 {
            self.selected = if up { (self.selected + len - 1) % len } else { (self.selected + 1) % len };
        }
    }

    /// 画面中央に枠付きのメニューを描く．
    pub fn draw(&self, frame: &mut Frame, tasks: &[TaskEntry]) {
        let Some(Geometry { x0, y0, w, h, lines }) = self.geometry(frame.w, frame.h, tasks) else { return };
        let (title, footer) = (TITLE, FOOTER);

        let border = Style { fg: Color::Green, ..Style::default() };
        let normal = Style::default();
        let sel = Style { fg: Color::Black, bg: Color::Green, ..Style::default() };

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
        put_clipped(frame, x0 + 2, y0, title, border, x0 + w - 1);
        put_clipped(frame, x0 + 1, y0 + h - 1, footer, border, x0 + w - 1);

        for (i, line) in lines.iter().enumerate().take(h as usize - 2) {
            let y = y0 + 1 + i as u16;
            let style = if !tasks.is_empty() && i == self.selected { sel } else { normal };
            for x in x0 + 1..x0 + w - 1 {
                frame.set(x, y, " ", style);
            }
            put_clipped(frame, x0 + 1, y, line, style, x0 + w - 1);
        }
    }
}

/// `end` の手前までに収まる分だけ書く（全角文字は 2 セル）．
fn put_clipped(frame: &mut Frame, x: u16, y: u16, s: &str, style: Style, end: u16) {
    let mut x = x;
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        let wide = ch.width() == Some(2);
        let need = if wide { 2 } else { 1 };
        if x + need > end {
            break;
        }
        frame.set(x, y, ch.encode_utf8(&mut buf), style);
        if wide {
            frame.set(x + 1, y, "", style);
        }
        x += need;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigate_and_launch() {
        let mut l = Launcher::default();
        assert!(matches!(l.feed(b"j", 3), MenuResult::Stay));
        assert!(matches!(l.feed(b"\x1b[B", 3), MenuResult::Stay));
        assert_eq!(l.selected, 2);
        assert!(matches!(l.feed(b"\x1b[B", 3), MenuResult::Stay));
        assert_eq!(l.selected, 0);
        assert!(matches!(l.feed(b"\r", 3), MenuResult::Launch(0)));
        assert!(matches!(l.feed(b"2", 3), MenuResult::Launch(1)));
        assert!(matches!(l.feed(b"\x1b", 3), MenuResult::Cancel));
    }
}
