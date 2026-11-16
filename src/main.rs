mod config;
mod deps;
mod help;
mod input;
mod launcher;
mod layout;
mod logfile;
mod mouse;
mod pane;
mod render;
mod scroll;

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use crossterm::style::Color;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use config::{Config, Restart, Task, TaskEntry};
use deps::{Dep, DepStatus, RunState, Wait};
use input::{Action, Direction, InputParser};
use help::Help;
use launcher::{Launcher, MenuResult, Selection};
use layout::{Dir, Node, PaneId, Rect, Separator};
use mouse::{Button, Kind as MouseKind, MouseEvent};
use pane::Pane;
use render::{Frame, Modes, Renderer, Style};

/// `end` の手前までに収まる分だけ書き，使ったセル数を返す（全角文字は 2 セル）．
fn put_clipped(frame: &mut Frame, x: u16, y: u16, s: &str, style: Style, end: u16) -> u16 {
    let mut cx = x;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0) as u16;
        if cw == 0 {
            continue;
        }
        if cx + cw > end {
            break;
        }
        frame.set(cx, y, ch.encode_utf8(&mut [0u8; 4]), style);
        if cw == 2 {
            frame.set(cx + 1, y, "", style);
        }
        cx += cw;
    }
    cx - x
}

/// 待機中のペインに出すボタンと，それに対応するキー（クリックで押せる）．
/// (ボタンの文字, 狭いときの短い文字, キー)．
const WAIT_BUTTONS: [(&str, &str, u8); 2] = [(" Enter  Start now ", " Enter Start ", b'\r'), (" q  Close ", " q Close ", b'q')];

/// 終了したペインの一番下に出すボタンと，それに対応するキー．
const EXIT_BUTTONS: [(&str, &str, u8); 3] =
    [(" r  Run again ", " r Run ", b'r'), (" s  Shell ", " s Shell ", b's'), (" q  Close ", " q Close ", b'q')];

const MOUSE_HINT: &str = "Mouse: on (Shift+drag to select text, C-q m to turn off)";

/// 起動したときにステータスバーに出す案内．
const START_HINT: &str = "New here? Press Ctrl-q, then ? to see all keys";

/// 幅 `width` に入るように切り詰める（切ったら末尾を … にする）．
fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    for ch in s.chars() {
        if out.width() + ch.width().unwrap_or(0) + 1 > width {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

/// ペインの中に重ねる 1 行のボタン列を組み立てる．部品は (左端からの列, 文字列, ボタンならキー)．
/// 幅に入らなければ，後ろの説明，前の説明を省き，ボタンを短い文字にし，最後は入りきらないボタンを省く．
fn button_bar(head: &str, buttons: &[(&str, &str, u8)], tail: &str, width: usize) -> Vec<(usize, String, Option<u8>)> {
    let build = |head: &str, tail: &str, short: bool| {
        let mut parts = Vec::new();
        let mut x = 1;
        if !head.is_empty() {
            parts.push((x, head.to_string(), None));
            x += head.width() + 2;
        }
        for (long, short_label, key) in buttons {
            let label = if short { short_label } else { long };
            parts.push((x, label.to_string(), Some(*key)));
            x += label.width() + 1;
        }
        if !tail.is_empty() {
            parts.push((x + 1, tail.to_string(), None));
        }
        parts
    };
    let end = |parts: &[(usize, String, Option<u8>)]| parts.last().map_or(0, |(x, t, _)| x + t.width());
    for (h, t, short) in [(head, tail, false), (head, "", false), ("", "", false), (head, "", true), ("", "", true)] {
        let parts = build(h, t, short);
        if end(&parts) <= width {
            return parts;
        }
    }
    let mut parts = build("", "", true);
    parts.retain(|(x, t, _)| x + t.width() <= width);
    parts
}

/// メインループに届くイベント．
pub enum Event {
    /// キーボードからの入力（生バイト列）．
    Input(Vec<u8>),
    /// いずれかのペインに出力があった．
    Output,
    /// ペインのプロセスが終了した（ペイン, 何回目の起動か）．
    Exited(PaneId, u64),
    /// 端末サイズが変わった．
    Resize,
}

/// raw モードと代替画面を有効にし，drop 時に元へ戻す．
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = write!(out, "{}\x1b[?1l\x1b>\x1b[?2004l\x1b[0m", mouse::DISABLE);
        let _ = execute!(out, LeaveAlternateScreen, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}


/// ウィンドウ = 画面全体を使うペインの集まり（tmux の window）．
struct Window {
    /// ステータスバーに出す名前（シェルなら "shell"，ランチャーからならプログラム名）．
    name: String,
    root: Option<Node>,
    active: PaneId,
    /// 直近のレイアウト結果（表示順）．各ペインの中身の領域（タイトル行を除く）．
    rects: Vec<(PaneId, Rect)>,
    /// 各ペインのタイトル行（中身の領域のすぐ上の 1 行）．
    titles: Vec<(PaneId, Rect)>,
    seps: Vec<Separator>,
}

impl Window {
    fn new(name: String, pane: PaneId) -> Self {
        Self { name, root: Some(Node::Leaf(pane)), active: pane, rects: Vec::new(), titles: Vec::new(), seps: Vec::new() }
    }

    fn rect_of(&self, id: PaneId) -> Option<Rect> {
        self.rects.iter().find(|(i, _)| *i == id).map(|(_, r)| *r)
    }

    fn contains(&self, id: PaneId) -> bool {
        self.rects.iter().any(|(i, _)| *i == id)
    }

    /// レイアウトを計算し直し，サイズが変わったペインの PTY を合わせる．
    fn relayout(&mut self, area: Rect, panes: &mut HashMap<PaneId, Pane>) {
        let old = std::mem::take(&mut self.rects);
        self.seps.clear();
        self.titles.clear();
        let mut full = Vec::new();
        if let Some(root) = &self.root {
            root.layout(area, &mut full, &mut self.seps);
        }
        // 各ペインの一番上の 1 行をタイトル行にする（高さが足りなければ付けない）
        for (id, r) in full {
            if r.h >= 2 {
                self.titles.push((id, Rect { h: 1, ..r }));
                self.rects.push((id, Rect { y: r.y + 1, h: r.h - 1, ..r }));
            } else {
                self.rects.push((id, r));
            }
        }
        for (id, r) in &self.rects {
            let changed = old.iter().find(|(i, _)| i == id).is_none_or(|(_, o)| (o.w, o.h) != (r.w, r.h));
            if changed && let Some(p) = panes.get_mut(id) {
                p.resize(r.h, r.w);
            }
        }
    }

    /// タイトル行を含めたペイン全体の領域．
    fn full_rect_of(&self, id: PaneId) -> Option<Rect> {
        let r = self.rect_of(id)?;
        Some(match self.titles.iter().find(|(i, _)| *i == id) {
            Some((_, t)) => Rect { y: t.y, h: r.h + 1, ..r },
            None => r,
        })
    }

    fn focus_next(&mut self) {
        if let Some(i) = self.rects.iter().position(|(id, _)| *id == self.active) {
            self.active = self.rects[(i + 1) % self.rects.len()].0;
        }
    }

    /// 指定方向に隣接するペインのうち，最も近いものへ移る．
    fn focus_dir(&mut self, d: Direction) {
        let Some(a) = self.rect_of(self.active) else { return };
        let overlap = |s1: u16, l1: u16, s2: u16, l2: u16| s1 < s2 + l2 && s2 < s1 + l1;
        let center = |s: u16, l: u16| s as i32 * 2 + l as i32;

        let best = self
            .rects
            .iter()
            .filter(|(id, _)| *id != self.active)
            .filter_map(|(id, r)| {
                let (ok, dist, off) = match d {
                    Direction::Left => (r.x + r.w <= a.x && overlap(r.y, r.h, a.y, a.h), a.x.saturating_sub(r.x + r.w), center(r.y, r.h) - center(a.y, a.h)),
                    Direction::Right => (r.x >= a.x + a.w && overlap(r.y, r.h, a.y, a.h), r.x.saturating_sub(a.x + a.w), center(r.y, r.h) - center(a.y, a.h)),
                    Direction::Up => (r.y + r.h <= a.y && overlap(r.x, r.w, a.x, a.w), a.y.saturating_sub(r.y + r.h), center(r.x, r.w) - center(a.x, a.w)),
                    Direction::Down => (r.y >= a.y + a.h && overlap(r.x, r.w, a.x, a.w), r.y.saturating_sub(a.y + a.h), center(r.x, r.w) - center(a.x, a.w)),
                };
                ok.then_some((dist, off.abs(), *id))
            })
            .min();
        if let Some((_, _, id)) = best {
            self.active = id;
        }
    }
}

/// ステータスバーのクリックできる部分．
#[derive(Clone, Copy, Debug)]
enum Click {
    Window(usize),
    Split(Dir),
    NewWindow,
    Launcher,
    Scroll,
    ExitScroll,
    Help,
}

/// ステータスバーに並べる 1 つの部品．
struct StatusItem {
    x: u16,
    text: String,
    style: Style,
    click: Option<Click>,
}

struct App {
    /// 全ウィンドウのペイン．
    panes: HashMap<PaneId, Pane>,
    windows: Vec<Window>,
    /// 表示中のウィンドウの添字．
    current: usize,
    next_id: PaneId,
    /// 端末全体のサイズ (cols, rows)．
    size: (u16, u16),
    input: InputParser,
    config: Config,
    /// 開いているランチャーメニュー．
    launcher: Option<Launcher>,
    /// 開いているヘルプ画面．
    help: Option<Help>,
    /// スクロールモード中か（アクティブなペインの過去の出力を見ている）．
    scrolling: bool,
    /// ステータスバーに出すエラー．次のキー入力かクリックで消える．
    message: Option<String>,
    /// ステータスバーに出すお知らせ．次のキー入力かクリックで消える．
    info: Option<String>,
    /// マウス操作を使うか．
    mouse: bool,
    /// 起動を待っているペイン（task.yaml の after / delay）．
    waits: HashMap<PaneId, Wait>,
    /// 各ペインのプログラムの状態（after の判定に使う）．
    runs: HashMap<PaneId, RunState>,
    /// 自動再起動を待っているペインと，再起動する時刻．
    restarts: HashMap<PaneId, Instant>,
    /// 各ペインで自動再起動した回数（max_restarts の判定に使う）．
    restart_counts: HashMap<PaneId, u32>,
    mouse_input: mouse::Extractor,
    tx: Sender<Event>,
    quit: bool,
}

impl App {
    /// `startup` はコマンドラインの `-t` で指定されたタスクの添字と，起動するものの名前（空なら全部）．
    /// 起動後はその最後のウィンドウを表示する．
    fn new(tx: Sender<Event>, config: Config, startup: &[(usize, Vec<String>)]) -> Result<Self> {
        let size = terminal::size()?;
        // マウス操作は常にオンで起動する（C-q m で切り替えられる）
        let mouse = true;
        let mut app = Self {
            panes: HashMap::new(),
            windows: Vec::new(),
            current: 0,
            next_id: 0,
            size,
            input: InputParser::default(),
            config,
            launcher: None,
            help: None,
            message: None,
            info: Some(START_HINT.to_string()),
            mouse,
            mouse_input: mouse::Extractor::default(),
            waits: HashMap::new(),
            runs: HashMap::new(),
            restarts: HashMap::new(),
            restart_counts: HashMap::new(),
            scrolling: false,
            tx,
            quit: false,
        };
        // -t で指定されたタスクを開く（表示は最後に開いたもの）
        for (i, names) in startup {
            app.launch_task_only(*i, names);
        }
        if app.windows.is_empty() {
            // タスクが 1 つも開かなかったときだけ，シェルのウィンドウを開く
            app.new_window()?;
        }
        Ok(app)
    }

    /// ペインを置ける領域（最下行はステータスバー）．
    fn pane_area(&self) -> Rect {
        let (w, h) = self.size;
        Rect { x: 0, y: 0, w, h: h.saturating_sub(1) }
    }

    fn win(&self) -> &Window {
        &self.windows[self.current]
    }

    fn win_mut(&mut self) -> &mut Window {
        &mut self.windows[self.current]
    }

    /// シェルを 1 つ持つウィンドウを末尾に作り，そこへ切り替える．
    fn new_window(&mut self) -> Result<()> {
        let area = self.pane_area();
        let id = self.next_id;
        let pane = Pane::spawn(id, area.h, area.w, None, self.tx.clone())?;
        self.next_id += 1;
        self.panes.insert(id, pane);
        let mut win = Window::new("shell".into(), id);
        win.relayout(area, &mut self.panes);
        self.windows.push(win);
        self.current = self.windows.len() - 1;
        Ok(())
    }

    /// shouji.yaml の `i` 番目のタスクを読み込んで起動する．失敗はステータスバーに出す．
    fn launch_task(&mut self, i: usize) {
        self.launch_task_only(i, &[]);
    }

    /// タスクのうち `names`（ウィンドウ名かプログラム名）で選んだものだけを起動する．空なら全部．
    fn launch_task_only(&mut self, i: usize, names: &[String]) {
        let Some(entry) = self.config.tasks.get(i).cloned() else { return };
        let result = entry.load().and_then(|mut task| {
            task.select(names)?;
            self.open_task(task)
        });
        if let Err(e) = result {
            self.report(e);
        }
    }

    /// タスクの各プログラムを，新しいウィンドウを均等に分割したペインで起動する．
    fn open_task(&mut self, task: Task) -> Result<()> {
        // after は別のウィンドウのプログラムも指せるので，先にタスク全体のペイン番号を決める
        let ids: Vec<PaneId> = (self.next_id..self.next_id + task.programs.len()).collect();
        self.next_id += ids.len();

        let area = self.pane_area();
        let mut failed = Vec::new();
        let mut first_window = None;
        for tw in &task.windows {
            let win_ids: Vec<PaneId> = tw.programs.iter().map(|&k| ids[k]).collect();
            let mut win = Window::new(tw.name.clone(), win_ids[0]);
            win.root = Some(Node::tiled(&win_ids, Dir::Horizontal));
            // 先に配置を決め，各ペインをその大きさで起動する
            win.relayout(area, &mut self.panes);
            for &k in &tw.programs {
                let (id, prog) = (ids[k], &task.programs[k]);
                let r = win.rect_of(id).unwrap();
                if prog.waits() {
                    // after / delay のあるものは，待機中のペインとして置いておく
                    let deps = prog
                        .after
                        .iter()
                        .map(|&(d, cond)| Dep { pane: ids[d], name: task.programs[d].name.clone(), cond })
                        .collect();
                    self.panes.insert(id, Pane::idle(r.h, r.w, Some(prog)));
                    self.waits.insert(id, Wait { deps, delay: prog.delay, ready_since: None });
                    continue;
                }
                match Pane::spawn(id, r.h, r.w, Some(prog), self.tx.clone()) {
                    Ok(p) => {
                        self.panes.insert(id, p);
                        self.mark_started(id);
                    }
                    Err(e) => {
                        failed.push(format!("{}: {e}", prog.name));
                        win.root = win.root.take().and_then(|r| r.remove(id));
                    }
                }
            }
            if win.root.is_none() {
                continue; // このウィンドウのプログラムがすべて起動できなかった
            }
            win.relayout(area, &mut self.panes);
            win.active = win.rects[0].0;
            self.windows.push(win);
            first_window.get_or_insert(self.windows.len() - 1);
        }
        let Some(first) = first_window else {
            bail!("cannot start {}: {}", task.name, failed.join(", "));
        };
        // 表示するのはタスクの最初のウィンドウ
        self.current = first;
        if !failed.is_empty() {
            self.message = Some(format!("failed to start: {}", failed.join(", ")));
        }
        Ok(())
    }

    fn relayout_current(&mut self) {
        let area = self.pane_area();
        self.windows[self.current].relayout(area, &mut self.panes);
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Input(bytes) => {
                let (keys, events) = self.mouse_input.feed(&bytes);
                for ev in events {
                    self.on_mouse(ev);
                }
                if !keys.is_empty() {
                    self.on_keys(&keys);
                }
            }
            Event::Output => {}
            Event::Exited(id, generation) => self.on_exit(id, generation),
            Event::Resize => {
                if let Ok(size) = terminal::size() {
                    self.size = size;
                    // 非表示のウィンドウも含めて，全ペインのサイズを合わせる
                    let area = self.pane_area();
                    for win in &mut self.windows {
                        win.relayout(area, &mut self.panes);
                    }
                }
            }
        }
    }

    fn on_keys(&mut self, bytes: &[u8]) {
        self.info = None;
        if let Some(h) = self.help.as_mut() {
            if h.feed(bytes, self.size.0, self.size.1) {
                self.help = None;
            }
        } else if let Some(l) = self.launcher.as_mut() {
            self.message = None;
            let result = l.feed(bytes, self.config.tasks.len());
            self.menu_result(result);
        } else if self.scrolling {
            self.scroll(bytes);
        } else {
            self.message = None;
            for action in self.input.feed(bytes) {
                self.do_action(action);
            }
        }
    }

    fn menu_result(&mut self, result: MenuResult) {
        match result {
            MenuResult::Stay => {}
            MenuResult::Cancel => self.launcher = None,
            MenuResult::Launch(i) => {
                self.launcher = None;
                self.launch_task(i);
            }
            MenuResult::OpenSelect(i) => {
                // 選択画面はタスクファイルを読んでから開く
                let Some(entry) = self.config.tasks.get(i).cloned() else { return };
                match entry.load() {
                    Ok(task) => {
                        if let Some(l) = self.launcher.as_mut() {
                            l.select = Some(Selection::new(task));
                        }
                    }
                    Err(e) => {
                        self.launcher = None;
                        self.report(e);
                    }
                }
            }
            MenuResult::LaunchTask(task) => {
                self.launcher = None;
                if let Err(e) = self.open_task(task) {
                    self.report(e);
                }
            }
            MenuResult::NothingSelected => self.message = Some("nothing selected (press Space to check items)".into()),
        }
    }

    /// エラーをステータスバーに出す．原因（ファイルが無い・不明な項目など）を先に，
    /// どのファイルかを後ろに付ける．
    fn report(&mut self, e: anyhow::Error) {
        let msg = match e.chain().count() {
            1 => format!("{e}"),
            _ => format!("{} [{e}]", e.root_cause()),
        };
        self.message = Some(msg.replace('\n', " "));
    }

    // ---- マウス ----

    fn on_mouse(&mut self, ev: MouseEvent) {
        if !self.mouse {
            return;
        }
        let (w, h) = self.size;
        let pressed = matches!(ev.kind, MouseKind::Press(_));

        if let Some(help) = self.help.as_mut() {
            match ev.kind {
                MouseKind::Press(_) if help.click_outside(ev.x, ev.y, w, h) => self.help = None,
                MouseKind::WheelUp => help.scroll(-3, w, h),
                MouseKind::WheelDown => help.scroll(3, w, h),
                _ => {}
            }
            return;
        }
        if let Some(l) = self.launcher.as_mut() {
            match ev.kind {
                MouseKind::Press(Button::Left) => {
                    let result = l.click(ev.x, ev.y, w, h, &self.config.tasks);
                    self.menu_result(result);
                }
                MouseKind::WheelUp => l.wheel(true, self.config.tasks.len()),
                MouseKind::WheelDown => l.wheel(false, self.config.tasks.len()),
                _ => {}
            }
            return;
        }

        if h > 0 && ev.y == h - 1 {
            // ステータスバー: 表示中の部品で判定してからメッセージを消す
            let click = if ev.kind == MouseKind::Press(Button::Left) { self.status_click(ev.x) } else { None };
            if pressed {
                self.message = None;
                self.info = None;
            }
            if let Some(c) = click {
                self.do_click(c);
            }
            return;
        }
        if pressed {
            self.message = None;
            self.info = None;
        }

        let Some(id) = self.pane_at(ev.x, ev.y) else { return };
        match ev.kind {
            MouseKind::Press(Button::Left) => {
                self.focus_pane(id);
                if let Some(key) = self.wait_button_at(id, ev.x, ev.y) {
                    self.waiting_key(id, key);
                } else {
                    self.click_dead_pane(id, ev.x, ev.y);
                }
            }
            MouseKind::WheelUp => self.wheel(id, true),
            MouseKind::WheelDown => self.wheel(id, false),
            _ => {}
        }
    }

    /// 表示中のウィンドウで (x, y) にあるペイン．
    fn pane_at(&self, x: u16, y: u16) -> Option<PaneId> {
        let win = self.win();
        win.rects.iter().chain(&win.titles).find(|(_, r)| r.contains(x, y)).map(|(id, _)| *id)
    }

    /// スクロールモードを抜け，表示を最新に戻す．
    fn stop_scrolling(&mut self) {
        if self.scrolling {
            if let Some(p) = self.panes.get(&self.win().active) {
                p.parser.lock().unwrap().screen_mut().set_scrollback(0);
            }
            self.scrolling = false;
        }
    }

    fn focus_pane(&mut self, id: PaneId) {
        if self.win().active != id {
            self.stop_scrolling();
            self.win_mut().active = id;
        }
    }

    /// ホイール: 全画面アプリ（less・top など）には矢印キーを送り，
    /// それ以外は過去の出力をスクロールする．
    fn wheel(&mut self, id: PaneId, up: bool) {
        const LINES: usize = 3;
        let Some(pane) = self.panes.get_mut(&id) else { return };
        let (alt, app_cursor) = {
            let p = pane.parser.lock().unwrap();
            (p.screen().alternate_screen(), p.screen().application_cursor())
        };
        if alt {
            if !pane.is_dead() {
                let seq: &[u8] = match (up, app_cursor) {
                    (true, true) => b"\x1bOA",
                    (true, false) => b"\x1b[A",
                    (false, true) => b"\x1bOB",
                    (false, false) => b"\x1b[B",
                };
                for _ in 0..LINES {
                    pane.write(seq);
                }
            }
            return;
        }

        self.focus_pane(id);
        let page = self.win().rect_of(id).map_or(1, |r| r.h as usize);
        let Some(pane) = self.panes.get(&id) else { return };
        let mut parser = pane.parser.lock().unwrap();
        let screen = parser.screen_mut();
        let cmd = if up { scroll::ScrollCmd::Up(LINES) } else { scroll::ScrollCmd::Down(LINES) };
        let next = scroll::apply(screen.scrollback(), &cmd, page, screen.scrollback_len());
        screen.set_scrollback(next);
        // 一番下まで戻ったらスクロールモードを抜ける
        self.scrolling = next > 0;
    }

    /// 終了したペインの一番下のボタン（r Run again など）をクリックしたら，そのキーを押したことにする．
    fn click_dead_pane(&mut self, id: PaneId, x: u16, y: u16) {
        let Some(r) = self.win().rect_of(id) else { return };
        let Some(parts) = self.exit_bar(id, r.w) else { return };
        if r.h == 0 || y != r.y + r.h - 1 {
            return;
        }
        let Some(col) = x.checked_sub(r.x) else { return };
        let col = col as usize;
        let key = parts.iter().find_map(|(px, t, key)| key.filter(|_| (*px..px + t.width()).contains(&col)));
        if let Some(key) = key {
            self.dead_pane_key(id, key);
        }
    }

    /// 終了したペインの一番下に出す案内とボタン（終了していなければ `None`）．
    fn exit_bar(&self, id: PaneId, width: u16) -> Option<Vec<(usize, String, Option<u8>)>> {
        let pane = self.panes.get(&id)?;
        if !pane.is_dead() || self.waits.contains_key(&id) {
            return None;
        }
        let head = match &pane.last_exit {
            _ if self.restarts.contains_key(&id) => "Stopped, restarting soon".to_string(),
            Some((true, _)) => "Finished".to_string(),
            Some((false, what)) => format!("Stopped ({what})"),
            None => "Stopped".to_string(),
        };
        Some(button_bar(&head, &EXIT_BUTTONS, "C-q [ or wheel: scroll back", width as usize))
    }

    fn status_click(&self, x: u16) -> Option<Click> {
        self.status_items()
            .into_iter()
            .find(|i| i.click.is_some() && x >= i.x && x < i.x + i.text.width() as u16)
            .and_then(|i| i.click)
    }

    fn do_click(&mut self, click: Click) {
        match click {
            Click::Window(i) if i < self.windows.len() => {
                self.stop_scrolling();
                self.current = i;
            }
            Click::Window(_) => {}
            Click::Split(dir) => {
                self.stop_scrolling();
                self.split(dir);
            }
            Click::NewWindow => {
                self.stop_scrolling();
                let _ = self.new_window();
            }
            Click::Launcher => {
                self.stop_scrolling();
                self.launcher = Some(Launcher::default());
            }
            Click::Scroll => self.scrolling = true,
            Click::ExitScroll => self.stop_scrolling(),
            Click::Help => {
                self.stop_scrolling();
                self.help = Some(Help::default());
            }
        }
    }

    /// 終了したペインでのキー: r で再起動，s でシェル，q で閉じる．
    fn dead_pane_key(&mut self, id: PaneId, key: u8) {
        let Some(p) = self.panes.get_mut(&id) else { return };
        // 手で操作したら自動再起動の予定は取りやめる（r は回数も数え直す）
        self.restarts.remove(&id);
        match key {
            b'r' => match p.restart(id, self.tx.clone()) {
                Ok(()) => {
                    self.restart_counts.remove(&id);
                    self.mark_started(id);
                }
                Err(e) => self.message = Some(format!("failed to restart: {e}")),
            },
            b's' => {
                if let Err(e) = p.open_shell(id, self.tx.clone()) {
                    self.message = Some(format!("cannot start a shell: {e}"));
                }
            }
            b'q' => self.remove_pane(id),
            _ => {}
        }
    }

    fn do_action(&mut self, action: Action) {
        let n = self.windows.len();
        match action {
            Action::Forward(bytes) => {
                let active = self.win().active;
                if self.waits.contains_key(&active) {
                    if let Some(&key) = bytes.iter().find(|b| b"\r\nq".contains(b)) {
                        self.waiting_key(active, key);
                    }
                    return;
                }
                let Some(p) = self.panes.get_mut(&active) else { return };
                if !p.is_dead() {
                    p.write(&bytes);
                } else if let Some(&key) = bytes.iter().find(|b| b"rsq".contains(b)) {
                    self.dead_pane_key(active, key);
                }
            }
            Action::Split(dir) => self.split(dir),
            Action::FocusNext => self.win_mut().focus_next(),
            Action::Focus(d) => self.win_mut().focus_dir(d),
            Action::Kill => {
                let active = self.win().active;
                match self.panes.get_mut(&active) {
                    Some(p) if p.is_dead() => self.remove_pane(active),
                    // 実際の削除は reader スレッドから Exited が届いたときに行う
                    Some(p) => p.kill(),
                    None => {}
                }
            }
            Action::ScrollMode => self.scrolling = true,
            Action::ToggleMouse => {
                self.mouse = !self.mouse;
                self.info = Some(if self.mouse { MOUSE_HINT.into() } else { "Mouse: off (C-q m to turn on)".into() });
            }
            Action::Quit => self.quit = true,
            Action::NewWindow => {
                let _ = self.new_window();
            }
            Action::OpenLauncher => self.launcher = Some(Launcher::default()),
            Action::Help => self.help = Some(Help::default()),
            Action::NextWindow => self.current = (self.current + 1) % n,
            Action::PrevWindow => self.current = (self.current + n - 1) % n,
            Action::SelectWindow(i) if i < n => self.current = i,
            Action::SelectWindow(_) => {}
        }
    }

    fn split(&mut self, dir: Dir) {
        let new = self.next_id;
        let win = self.win_mut();
        let active = win.active;
        let Some(r) = win.rect_of(active) else { return };
        // 分割後に各ペインが 1 セル以上になるか
        let too_small = match dir {
            Dir::Horizontal => r.w < 3,
            Dir::Vertical => r.h < 4,
        };
        let Some(root) = win.root.as_mut() else { return };
        if too_small || !root.split(active, new, dir) {
            return;
        }
        self.next_id += 1;
        self.relayout_current();

        let nr = self.win().rect_of(new).unwrap();
        match Pane::spawn(new, nr.h, nr.w, None, self.tx.clone()) {
            Ok(p) => {
                self.panes.insert(new, p);
                self.win_mut().active = new;
            }
            Err(_) => {
                let win = self.win_mut();
                win.root = win.root.take().and_then(|r| r.remove(new));
                self.relayout_current();
            }
        }
    }

    /// プロセスが終了した: 設定に応じてペインを残すか閉じる．
    fn on_exit(&mut self, id: PaneId, generation: u64) {
        let Some(pane) = self.panes.get_mut(&id) else { return };
        if pane.generation != generation {
            return; // 再起動前のプロセスからの古い通知
        }
        let keep = pane.keep_on_exit();
        let closing = pane.is_closing();
        let program = pane.program().cloned();
        let Some(exit) = pane.mark_exited() else { return };
        if exit.was_program {
            self.runs.entry(id).or_default().exit = Some(exit.success);
        }

        // 自動再起動（restart）．C-q x で閉じたときと autostart: false のペインは対象外
        let wants_restart = exit.was_program
            && !closing
            && program.as_ref().is_some_and(|p| {
                p.autostart
                    && match p.restart {
                        Restart::Never => false,
                        Restart::OnFailure => !exit.success,
                        Restart::Always => true,
                    }
            });
        if let (true, Some(p)) = (wants_restart, &program) {
            let count = self.restart_counts.get(&id).copied().unwrap_or(0);
            pane.show_exit(&exit.what);
            if p.max_restarts.is_none_or(|max| count < max) {
                let limit = p.max_restarts.map_or(String::new(), |max| format!(" of {max}"));
                pane.note(&format!(
                    "Restarting in {}s (attempt {}{limit}). Press q to cancel.",
                    p.restart_delay.as_secs_f64(),
                    count + 1
                ));
                self.restarts.insert(id, Instant::now() + p.restart_delay);
                self.restart_counts.insert(id, count + 1);
            } else {
                pane.note(&format!("Not restarting again: it already restarted {count} times (restart.max)."));
            }
            return;
        }

        if keep {
            pane.show_exit(&exit.what);
        } else {
            self.remove_pane(id);
        }
    }

    /// 時刻になった自動再起動を実行する．
    fn run_restarts(&mut self) {
        let now = Instant::now();
        let due: Vec<PaneId> = self.restarts.iter().filter(|(_, at)| **at <= now).map(|(id, _)| *id).collect();
        for id in due {
            self.restarts.remove(&id);
            let Some(pane) = self.panes.get_mut(&id) else { continue };
            match pane.restart(id, self.tx.clone()) {
                Ok(()) => self.mark_started(id),
                Err(e) => self.message = Some(format!("failed to restart: {e}")),
            }
        }
    }

    fn mark_started(&mut self, id: PaneId) {
        self.runs.insert(id, RunState { started: true, exit: None });
    }

    // ---- 起動順（after / delay） ----

    /// 条件がそろった待機中のペインを起動する．起動したことで別のペインの
    /// 条件（started）がそろうことがあるので，起動するものがなくなるまで繰り返す．
    fn check_waits(&mut self) {
        self.run_restarts();
        // ペインが抱えている問題（ログファイルを開けないなど）を表示する
        for pane in self.panes.values_mut() {
            if let Some(w) = pane.warning.take() {
                self.message = Some(w);
            }
        }
        loop {
            let now = Instant::now();
            let panes = &self.panes;
            let runs = &self.runs;
            let ready: Vec<PaneId> = self
                .waits
                .iter_mut()
                .filter_map(|(id, w)| w.poll(now, runs, |p| panes.contains_key(&p)).then_some(*id))
                .collect();
            if ready.is_empty() {
                return;
            }
            for id in ready {
                self.start_waiting(id);
            }
        }
    }

    /// 次に check_waits が必要になる時刻（delay の待ち）．
    fn next_deadline(&self) -> Option<Instant> {
        self.waits.values().filter_map(Wait::deadline).chain(self.restarts.values().copied()).min()
    }

    /// 待機中のペインを（条件にかかわらず）今すぐ起動する．
    fn start_waiting(&mut self, id: PaneId) {
        self.waits.remove(&id);
        let Some(pane) = self.panes.get_mut(&id) else { return };
        match pane.launch(id, self.tx.clone()) {
            Ok(()) => self.mark_started(id),
            Err(e) => self.message = Some(format!("failed to start: {e}")),
        }
    }

    /// 待機中のペインに表示する内容（ボタンの行は含まない．その 1 行下に wait_bar を出す）．
    fn wait_lines(&self, w: &Wait) -> Vec<(String, Style)> {
        let head = Style { fg: Color::Yellow, bold: true, ..Style::default() };
        let ok = Style { fg: Color::Green, ..Style::default() };
        let wait = Style::default();
        let ng = Style { fg: Color::Red, bold: true, ..Style::default() };
        let dim = Style { fg: Color::DarkGrey, ..Style::default() };

        let mut lines = vec![(" Waiting to start".to_string(), head)];
        let mut blocked = false;
        for d in &w.deps {
            let st = deps::status(d, &self.runs, self.panes.contains_key(&d.pane));
            let n = &d.name;
            let (mark, style, text) = match (st, d.cond) {
                (DepStatus::Met, config::Cond::Done) => ("ready", ok, format!("{n} finished successfully")),
                (DepStatus::Met, config::Cond::Started) => ("ready", ok, format!("{n} has started")),
                (DepStatus::Waiting, config::Cond::Done) => ("waiting", wait, format!("until {n} finishes successfully")),
                (DepStatus::Waiting, config::Cond::Started) => ("waiting", wait, format!("until {n} starts")),
                (DepStatus::Blocked(why), _) => {
                    blocked = true;
                    ("blocked", ng, format!("{n} {why}"))
                }
            };
            lines.push((format!("   {mark:<8} {text}"), style));
        }
        if !w.delay.is_zero() {
            let state = if w.ready_since.is_some() { " (counting down)" } else { "" };
            lines.push((format!("   then     wait {}s{state}", w.delay.as_secs_f64()), wait));
        }
        if blocked {
            lines.push(("   It will not start by itself.".to_string(), dim));
        }
        lines
    }

    /// 待機中のペインのボタン行（行の位置, 部品）．
    fn wait_bar(&self, w: &Wait, width: u16) -> (u16, Vec<(usize, String, Option<u8>)>) {
        let row = self.wait_lines(w).len() as u16 + 1;
        (row, button_bar("", &WAIT_BUTTONS, "", width as usize))
    }

    /// 待機中のペインのボタンがクリックされたか調べ，押されたキーを返す．
    fn wait_button_at(&self, id: PaneId, x: u16, y: u16) -> Option<u8> {
        let (w, r) = (self.waits.get(&id)?, self.win().rect_of(id)?);
        let (row, parts) = self.wait_bar(w, r.w);
        if y.checked_sub(r.y)? != row {
            return None;
        }
        let col = x.checked_sub(r.x)? as usize;
        parts.iter().find_map(|(px, t, key)| key.filter(|_| (*px..px + t.width()).contains(&col)))
    }

    /// 待機中のペインでのキー: Enter で今すぐ起動，q で閉じる．
    fn waiting_key(&mut self, id: PaneId, key: u8) {
        match key {
            b'\r' | b'\n' => self.start_waiting(id),
            b'q' => self.remove_pane(id),
            _ => {}
        }
    }

    /// スクロールモードのキー入力を処理する．
    fn scroll(&mut self, bytes: &[u8]) {
        let active = self.win().active;
        let page = self.win().rect_of(active).map_or(1, |r| r.h as usize);
        let Some(pane) = self.panes.get(&active) else {
            self.scrolling = false;
            return;
        };
        let mut parser = pane.parser.lock().unwrap();
        let screen = parser.screen_mut();
        for cmd in scroll::parse(bytes) {
            let max = screen.scrollback_len();
            let next = scroll::apply(screen.scrollback(), &cmd, page, max);
            screen.set_scrollback(next);
            if cmd == scroll::ScrollCmd::Exit {
                self.scrolling = false;
            }
        }
    }

    fn remove_pane(&mut self, id: PaneId) {
        self.waits.remove(&id);
        self.restarts.remove(&id);
        self.restart_counts.remove(&id);
        let Some(mut pane) = self.panes.remove(&id) else { return };
        pane.reap();
        let Some(wi) = self.windows.iter().position(|w| w.contains(id)) else { return };

        let area = self.pane_area();
        let win = &mut self.windows[wi];
        let idx = win.rects.iter().position(|(i, _)| *i == id).unwrap_or(0);
        win.root = win.root.take().and_then(|r| r.remove(id));
        win.relayout(area, &mut self.panes);

        if win.rects.is_empty() {
            // ウィンドウ最後のペインだったので，ウィンドウごと消す
            self.windows.remove(wi);
            if self.windows.is_empty() {
                self.quit = true;
                return;
            }
            if wi < self.current || self.current >= self.windows.len() {
                self.current -= 1;
            }
        } else if win.active == id {
            // 表示順で一つ前のペインに移る
            win.active = win.rects[idx.saturating_sub(1).min(win.rects.len() - 1)].0;
        }
    }

    /// ステータスバーの部品と位置（描画とクリック判定で共通）．
    /// 左: [shouji] 0:shell* 1:htop ...（番号:名前，* が表示中）とお知らせ，右: ボタンかヘルプ．
    fn status_items(&self) -> Vec<StatusItem> {
        let (w, _) = self.size;
        let bar = Style { fg: Color::Black, bg: Color::Green, ..Style::default() };
        let cur = Style { fg: Color::Green, bg: Color::Black, bold: true, ..Style::default() };
        let btn = Style { fg: Color::Black, bg: Color::White, ..Style::default() };
        let yellow = Style { fg: Color::Black, bg: Color::Yellow, bold: true, ..Style::default() };
        let err = Style { fg: Color::White, bg: Color::Red, bold: true, ..Style::default() };
        let note = Style { fg: Color::Black, bg: Color::Cyan, ..Style::default() };

        let mut items = Vec::new();
        let mut x = 0u16;
        fn push(items: &mut Vec<StatusItem>, x: &mut u16, text: String, style: Style, click: Option<Click>) {
            let width = text.width() as u16;
            items.push(StatusItem { x: *x, text, style, click });
            *x = x.saturating_add(width);
        }
        push(&mut items, &mut x, " shouji ".into(), Style { bold: true, ..bar }, None);
        for (i, wn) in self.windows.iter().enumerate() {
            let mark = if i == self.current { "*" } else { "" };
            let style = if i == self.current { cur } else { bar };
            push(&mut items, &mut x, format!(" {i}:{}{mark} ", wn.name), style, Some(Click::Window(i)));
        }
        let buttons = || -> Vec<(String, Style, Option<Click>)> {
            // 幅が足りなければ後ろから省くので，大事なものを前に置く
            [
                (" ? Help ", Click::Help),
                (" Tasks ", Click::Launcher),
                (" Split | ", Click::Split(Dir::Horizontal)),
                (" Split - ", Click::Split(Dir::Vertical)),
                (" New window ", Click::NewWindow),
                (" Scroll ", Click::Scroll),
            ]
            .into_iter()
            .map(|(t, c)| (t.to_string(), btn, Some(c)))
            .collect()
        };
        // ウィンドウ一覧の右に出す 1 つの知らせ（プレフィックス・スクロール中・エラー・お知らせ）と，右寄せのボタン
        let mut right: Vec<(String, Style, Option<Click>)> = Vec::new();
        let note_item = if self.input.in_prefix() {
            right = if self.mouse { buttons() } else { Vec::new() };
            Some((" Ctrl-q pressed: now press a key (? for help) ".to_string(), yellow))
        } else if self.scrolling {
            let (off, len) = self.panes.get(&self.win().active).map_or((0, 0), |p| {
                let parser = p.parser.lock().unwrap();
                (parser.screen().scrollback(), parser.screen().scrollback_len())
            });
            if self.mouse {
                right.push((" Stop scrolling ".into(), btn, Some(Click::ExitScroll)));
            }
            Some((format!(" SCROLLING {off}/{len} lines back  k/j u/d g/G, q: stop "), yellow))
        } else {
            if self.mouse {
                right = buttons();
            } else {
                right.push((" C-q Space: tasks  C-q ?: help ".into(), bar, None));
            }
            match (&self.message, &self.info) {
                (Some(msg), _) => Some((format!(" {msg} "), err)),
                (None, Some(info)) => Some((format!(" {info} "), note)),
                (None, None) => None,
            }
        };

        // 幅が足りなければ，まずボタンを後ろから（先頭の ? Help は残す）省き，次に知らせを切り詰める
        let total = |r: &[(String, Style, Option<Click>)]| -> u16 {
            r.iter().map(|(t, _, _)| t.width() as u16 + 1).sum::<u16>().saturating_sub(1)
        };
        let note_w = note_item.as_ref().map_or(0, |(t, _)| t.width() as u16 + 1);
        while right.len() > 1 && x as u32 + note_w as u32 + 1 + total(&right) as u32 > w as u32 {
            right.pop();
        }
        if right.len() == 1 && right[0].2.is_none() && x as u32 + note_w as u32 + 1 + total(&right) as u32 > w as u32 {
            right[0].0 = " C-q ?: help ".into();
        }
        while !right.is_empty() && x as u32 + 1 + total(&right) as u32 > w as u32 {
            right.pop();
        }
        if let Some((text, style)) = note_item {
            let room = (w as usize).saturating_sub(x as usize + 2 + total(&right) as usize);
            if room >= 8 {
                push(&mut items, &mut x, " ".into(), bar, None);
                push(&mut items, &mut x, truncate(&text, room), style, None);
            }
        }
        let mut rx = w.saturating_sub(total(&right));
        for (text, style, click) in right {
            let width = text.width() as u16;
            items.push(StatusItem { x: rx, text, style, click });
            rx = rx.saturating_add(width + 1);
        }
        items
    }

    /// ペインのタイトルに付ける状態の札．設定のプログラムでないシェルのペインには付けない．
    fn badge(&self, id: PaneId, pane: &Pane) -> Option<(String, Style)> {
        let style = |fg, bg| Style { fg, bg, bold: true, ..Style::default() };
        let (text, style) = if self.waits.contains_key(&id) {
            ("WAITING".to_string(), style(Color::Black, Color::Yellow))
        } else if self.restarts.contains_key(&id) {
            ("RESTARTING".to_string(), style(Color::Black, Color::Yellow))
        } else if pane.is_dead() {
            match &pane.last_exit {
                Some((true, _)) => ("DONE".to_string(), style(Color::Black, Color::Cyan)),
                Some((false, what)) => (format!("FAILED ({what})"), style(Color::White, Color::Red)),
                None => ("STOPPED".to_string(), style(Color::White, Color::Red)),
            }
        } else if pane.running_shell() {
            ("SHELL".to_string(), style(Color::Black, Color::White))
        } else if pane.program().is_some() {
            ("RUNNING".to_string(), style(Color::Green, Color::Black))
        } else {
            return None;
        };
        Some((format!(" {text} "), style))
    }

    /// 現在の状態からフレームを組み立てる．
    fn compose(&self) -> (Frame, Option<(u16, u16)>, Modes) {
        let (w, h) = self.size;
        let mut frame = Frame::new(w, h);
        let mut cursor = None;
        let mut modes = Modes::default();
        let win = self.win();

        for (id, r) in &win.rects {
            let Some(pane) = self.panes.get(id) else { continue };
            let parser = pane.parser.lock().unwrap();
            let screen = parser.screen();
            for row in 0..r.h {
                for col in 0..r.w {
                    let Some(cell) = screen.cell(row, col) else { continue };
                    let style = Style::from_cell(cell);
                    let text = if cell.is_wide_continuation() {
                        ""
                    } else if !cell.has_contents() || (cell.is_wide() && col + 1 >= r.w) {
                        " "
                    } else {
                        cell.contents()
                    };
                    frame.set(r.x + col, r.y + row, text, style);
                }
            }
            let end = r.x + r.w;
            let button = Style { fg: Color::Black, bg: Color::White, bold: true, ..Style::default() };
            if let Some(w) = self.waits.get(id) {
                for (i, (line, style)) in self.wait_lines(w).iter().enumerate().take(r.h as usize) {
                    put_clipped(&mut frame, r.x, r.y + i as u16, line, *style, end);
                }
                let (row, parts) = self.wait_bar(w, r.w);
                if row < r.h {
                    for (x, text, _) in parts {
                        put_clipped(&mut frame, r.x + x as u16, r.y + row, &text, button, end);
                    }
                }
            } else if let (Some(parts), true) = (self.exit_bar(*id, r.w), r.h > 0) {
                // 終了したペイン: 一番下の行に案内とボタンを重ねる
                let y = r.y + r.h - 1;
                let bar = Style { fg: Color::White, bg: Color::DarkGrey, ..Style::default() };
                let head = match pane.last_exit {
                    Some((true, _)) => Style { fg: Color::Cyan, bg: Color::DarkGrey, bold: true, ..Style::default() },
                    _ => Style { fg: Color::Red, bg: Color::DarkGrey, bold: true, ..Style::default() },
                };
                for x in r.x..end {
                    frame.set(x, y, " ", bar);
                }
                for (i, (x, text, key)) in parts.iter().enumerate() {
                    let style = match (key, i) {
                        (Some(_), _) => button,
                        (None, 0) if !text.starts_with("C-q") => head,
                        _ => Style { fg: Color::Grey, ..bar },
                    };
                    put_clipped(&mut frame, r.x + *x as u16, y, text, style, end);
                }
            }
            if *id == win.active {
                // 終了したペインとスクロール中はカーソルを出さない
                if !screen.hide_cursor() && !pane.is_dead() && !self.scrolling {
                    let (cr, cc) = screen.cursor_position();
                    cursor = Some((r.x + cc.min(r.w.saturating_sub(1)), r.y + cr.min(r.h.saturating_sub(1))));
                }
                modes = Modes {
                    app_cursor: screen.application_cursor(),
                    app_keypad: screen.application_keypad(),
                    bracketed_paste: screen.bracketed_paste(),
                    mouse: false,
                };
            }
        }

        // 区切り線: アクティブなペインに接する部分を緑にする
        let active = win.full_rect_of(win.active);
        let touches = |x: u16, y: u16, dir: Dir| {
            active.is_some_and(|a| match dir {
                Dir::Horizontal => a.contains(x.wrapping_sub(1), y) || a.contains(x + 1, y),
                Dir::Vertical => a.contains(x, y.wrapping_sub(1)) || a.contains(x, y + 1),
            })
        };
        for s in &win.seps {
            for i in 0..s.len {
                let (x, y, ch) = match s.dir {
                    Dir::Horizontal => (s.x, s.y + i, "│"),
                    Dir::Vertical => (s.x + i, s.y, "─"),
                };
                let fg = if touches(x, y, s.dir) { Color::Green } else { Color::DarkGrey };
                frame.set(x, y, ch, Style { fg, ..Style::default() });
            }
        }

        modes.mouse = self.mouse;

        // ペインのタイトル行: 名前と実行中のコマンド（アクティブなペインは緑）
        for (id, t) in &win.titles {
            let active = *id == win.active;
            let (bar, dim) = if active {
                (Style { fg: Color::Black, bg: Color::Green, bold: true, ..Style::default() }, Style { fg: Color::Black, bg: Color::Green, ..Style::default() })
            } else {
                (Style { fg: Color::White, bg: Color::DarkGrey, bold: true, ..Style::default() }, Style { fg: Color::Grey, bg: Color::DarkGrey, ..Style::default() })
            };
            for x in t.x..t.x + t.w {
                frame.set(x, t.y, " ", bar);
            }
            let pane = self.panes.get(id);
            let program = pane.and_then(Pane::program);
            let name = program.map_or("shell", |p| p.name.as_str());
            let end = t.x + t.w;
            let mut x = t.x + put_clipped(&mut frame, t.x, t.y, &format!(" {name} "), bar, end);
            // 状態の札（色付き）: RUNNING / WAITING / RESTARTING / DONE / FAILED / SHELL
            if let Some((text, style)) = pane.and_then(|p| self.badge(*id, p)) {
                x += put_clipped(&mut frame, x, t.y, &text, style, end);
                x += put_clipped(&mut frame, x, t.y, " ", bar, end);
            }
            // PID（外から kill するとき用）．前面のプロセスが別なら fg も出す
            if let Some((pid, fg)) = pane.and_then(Pane::pids) {
                let text = match fg {
                    Some(fg) if fg != pid => format!("│ pid {pid} fg {fg} "),
                    _ => format!("│ pid {pid} "),
                };
                x += put_clipped(&mut frame, x, t.y, &text, dim, end);
            }
            if let Some(p) = program {
                let command = p.command.split_whitespace().collect::<Vec<_>>().join(" ");
                x += put_clipped(&mut frame, x, t.y, "│ ", dim, end);
                put_clipped(&mut frame, x, t.y, &command, dim, end);
            }
        }

        if let Some(l) = &self.launcher {
            l.draw(&mut frame, &self.config.tasks);
            cursor = None;
        }
        if let Some(help) = &self.help {
            help.draw(&mut frame);
            cursor = None;
        }

        // ステータスバー
        if h > 0 {
            let bar = Style { fg: Color::Black, bg: Color::Green, ..Style::default() };
            for x in 0..w {
                frame.set(x, h - 1, " ", bar);
            }
            for item in self.status_items() {
                frame.put_str(item.x, h - 1, &item.text, item.style);
            }
        }

        (frame, cursor, modes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_bar_shrinks() {
        let labels = |parts: Vec<(usize, String, Option<u8>)>| parts.into_iter().map(|(_, t, _)| t).collect::<Vec<_>>().join("|");
        // 広ければ全部
        assert_eq!(labels(button_bar("Stopped", &EXIT_BUTTONS, "tail", 100)), "Stopped| r  Run again | s  Shell | q  Close |tail");
        // 後ろの説明から省く
        assert_eq!(labels(button_bar("Stopped", &EXIT_BUTTONS, "tail", 50)), "Stopped| r  Run again | s  Shell | q  Close ");
        // ボタン 3 つは短い文字で必ず入れる
        assert_eq!(labels(button_bar("Stopped (code 1)", &EXIT_BUTTONS, "tail", 30)), " r Run | s Shell | q Close ");
        // それでも入らなければ入る分だけ
        assert_eq!(labels(button_bar("x", &EXIT_BUTTONS, "", 12)), " r Run ");
        // 位置は 1 列目から，間は 1 列
        let parts = button_bar("", &WAIT_BUTTONS, "", 100);
        assert_eq!((parts[0].0, parts[1].0), (1, 1 + " Enter  Start now ".width() + 1));
    }
}

fn main() -> Result<()> {
    if std::env::var_os("SHOUJI").is_some() {
        bail!("shouji cannot be started inside shouji (SHOUJI is set)");
    }

    // 設定は raw モードに入る前に読む（エラーを普通に表示するため）
    const USAGE: &str = "usage: shouji [-c CONFIG.yaml] [-t TASK.yaml [NAME...]]...\n\
        \n  -c, --config FILE     config file (default: ./shouji.yaml, then ~/.config/shouji/config.yaml)\
        \n  -t, --task FILE       task file to open at startup (can be repeated)\
        \n                        NAMEs (window or program names) after it start only those\
        \n                        e.g. shouji -t task.yaml app           every program in the app window\
        \n                             shouji -t task.yaml build server  only build and server";
    let mut config_path = None;
    // (タスクファイル, 起動するものの名前)
    let mut task_args: Vec<(String, Vec<String>)> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => config_path = Some(std::path::PathBuf::from(args.next().context(USAGE)?)),
            "-t" | "--task" => task_args.push((args.next().context(USAGE)?, Vec::new())),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if arg.starts_with('-') => bail!("unknown argument: {arg}\n{USAGE}"),
            // -t FILE に続く名前
            _ => match task_args.last_mut() {
                Some((_, names)) => names.push(arg),
                None => bail!("{arg}: names must come after -t TASK.yaml\n{USAGE}"),
            },
        }
    }
    let mut config = Config::load(config_path.as_deref())?;

    // -t のタスクはここで一度読んで，書式の誤りを raw モードに入る前に報告する．
    // メニューから開き直せるよう，タスク一覧にも加える．
    let cwd = std::env::current_dir()?;
    let mut startup = Vec::new();
    for (path, names) in task_args {
        let entry = TaskEntry::new(&path, None, &cwd);
        // 名前の間違いもここで報告する
        entry.load()?.select(&names).with_context(|| format!("cannot select programs in {path}"))?;
        startup.push((config.add_task(entry), names));
    }

    let (tx, rx) = mpsc::channel();

    // キーボード入力
    {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut stdin = io::stdin();
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 || tx.send(Event::Input(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        });
    }

    // 端末のリサイズ通知
    {
        let tx = tx.clone();
        let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
        thread::spawn(move || {
            for _ in signals.forever() {
                if tx.send(Event::Resize).is_err() {
                    break;
                }
            }
        });
    }

    let guard = TerminalGuard::new()?;
    let mut app = App::new(tx, config, &startup)?;
    let mut renderer = Renderer::default();
    let mut out = io::BufWriter::with_capacity(1 << 16, io::stdout());

    loop {
        app.check_waits();
        let (frame, cursor, modes) = app.compose();
        renderer.draw(&mut out, frame, cursor, modes)?;

        // delay の待ちがあれば，その時刻までにイベントがなくても起き上がる
        let ev = match app.next_deadline() {
            Some(deadline) => match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(ev) => Some(ev),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(ev) => Some(ev),
                Err(_) => break,
            },
        };
        let Some(ev) = ev else { continue };
        let mut resized = matches!(ev, Event::Resize);
        app.handle(ev);
        // 溜まっているイベントをまとめて処理してから 1 回だけ描画する
        while let Ok(ev) = rx.try_recv() {
            resized |= matches!(ev, Event::Resize);
            app.handle(ev);
        }
        if resized {
            renderer.invalidate();
        }
        if app.quit {
            break;
        }
    }

    for pane in app.panes.values_mut() {
        pane.kill();
    }
    drop(guard);
    Ok(())
}
