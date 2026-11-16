//! 画面全体のフレームバッファと，前回フレームとの差分描画．

use std::io::Write;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::queue;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg: Color::Reset,
            bg: Color::Reset,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
        }
    }
}

impl Style {
    pub fn from_cell(cell: &vt100::Cell) -> Self {
        Self {
            fg: conv_color(cell.fgcolor()),
            bg: conv_color(cell.bgcolor()),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }
}

fn conv_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::AnsiValue(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb { r, g, b },
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Cell {
    /// 表示する文字．全角文字の右半分（継続セル）は空文字列．
    pub text: String,
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self { text: " ".into(), style: Style::default() }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Modes {
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub bracketed_paste: bool,
    /// 外側の端末にマウス報告を頼むか．
    pub mouse: bool,
}

pub struct Frame {
    pub w: u16,
    pub h: u16,
    cells: Vec<Cell>,
}

impl Frame {
    pub fn new(w: u16, h: u16) -> Self {
        Self { w, h, cells: vec![Cell::default(); w as usize * h as usize] }
    }

    pub fn set(&mut self, x: u16, y: u16, text: &str, style: Style) {
        if x < self.w && y < self.h {
            let c = &mut self.cells[y as usize * self.w as usize + x as usize];
            c.text.clear();
            c.text.push_str(text);
            c.style = style;
        }
    }

    /// 全体を暗くする（ヘルプなどを手前に出すとき，後ろを目立たなくする）．
    pub fn dim_all(&mut self, fg: Color, bg: Color) {
        for c in &mut self.cells {
            c.style = Style { fg, bg, ..Style::default() };
        }
    }

    fn get(&self, x: u16, y: u16) -> &Cell {
        &self.cells[y as usize * self.w as usize + x as usize]
    }
}

#[derive(Default)]
pub struct Renderer {
    prev: Option<Frame>,
    modes: Option<Modes>,
}

impl Renderer {
    /// 次回の描画で全セルを描き直す（端末サイズ変更時など）．
    pub fn invalidate(&mut self) {
        self.prev = None;
    }

    pub fn draw(
        &mut self,
        out: &mut impl Write,
        frame: Frame,
        cursor: Option<(u16, u16)>,
        modes: Modes,
    ) -> std::io::Result<()> {
        queue!(out, BeginSynchronizedUpdate, Hide)?;

        let prev = self.prev.take().filter(|p| p.w == frame.w && p.h == frame.h);
        if prev.is_none() {
            queue!(out, SetAttribute(Attribute::Reset), crossterm::terminal::Clear(crossterm::terminal::ClearType::All))?;
        }

        let mut style: Option<Style> = None;
        let mut pos: Option<(u16, u16)> = None;
        for y in 0..frame.h {
            for x in 0..frame.w {
                let cell = frame.get(x, y);
                if prev.as_ref().is_some_and(|p| p.get(x, y) == cell) || cell.text.is_empty() {
                    continue;
                }
                if pos != Some((x, y)) {
                    queue!(out, MoveTo(x, y))?;
                }
                if style != Some(cell.style) {
                    apply_style(out, cell.style)?;
                    style = Some(cell.style);
                }
                queue!(out, Print(&cell.text))?;
                // 全角文字なら次のセルが継続セルになっている
                let wide = x + 1 < frame.w && frame.get(x + 1, y).text.is_empty();
                pos = Some((x + if wide { 2 } else { 1 }, y));
            }
        }
        queue!(out, SetAttribute(Attribute::Reset))?;

        if self.modes != Some(modes) {
            let flag = |b: bool| if b { 'h' } else { 'l' };
            write!(out, "\x1b[?1{}", flag(modes.app_cursor))?;
            write!(out, "{}", if modes.app_keypad { "\x1b=" } else { "\x1b>" })?;
            write!(out, "\x1b[?2004{}", flag(modes.bracketed_paste))?;
            write!(out, "{}", if modes.mouse { crate::mouse::ENABLE } else { crate::mouse::DISABLE })?;
            self.modes = Some(modes);
        }

        if let Some((x, y)) = cursor {
            queue!(out, MoveTo(x, y), Show)?;
        }
        queue!(out, EndSynchronizedUpdate)?;
        out.flush()?;

        self.prev = Some(frame);
        Ok(())
    }
}

fn apply_style(out: &mut impl Write, s: Style) -> std::io::Result<()> {
    queue!(out, SetAttribute(Attribute::Reset), SetForegroundColor(s.fg), SetBackgroundColor(s.bg))?;
    for (on, attr) in [
        (s.bold, Attribute::Bold),
        (s.dim, Attribute::Dim),
        (s.italic, Attribute::Italic),
        (s.underline, Attribute::Underlined),
        (s.inverse, Attribute::Reverse),
    ] {
        if on {
            queue!(out, SetAttribute(attr))?;
        }
    }
    Ok(())
}
