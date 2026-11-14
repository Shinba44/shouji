mod config;
mod deps;
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
use launcher::{Launcher, MenuResult};
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
const WAIT_BUTTONS: [(&str, u8); 2] = [("[Enter: start now]", b'\r'), ("[q: close]", b'q')];

const MOUSE_HINT: &str = "マウス操作: オン（文字の選択は Shift+ドラッグ / C-q m で切替）";

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
    /// `startup` はコマンドラインの `-t` で指定されたタスクの添字．起動後はその最後のウィンドウを表示する．
    fn new(tx: Sender<Event>, config: Config, startup: &[usize]) -> Result<Self> {
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
            message: None,
            info: mouse.then(|| MOUSE_HINT.to_string()),
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
        for &i in startup {
            app.launch_task(i);
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
        let Some(entry) = self.config.tasks.get(i).cloned() else { return };
        if let Err(e) = entry.load().and_then(|task| self.open_task(task)) {
            // 原因（ファイルが無い・不明な項目など）を先に出し，どのファイルかを後ろに付ける
            let msg = match e.chain().count() {
                1 => format!("{e}"),
                _ => format!("{} [{e}]", e.root_cause()),
            };
            self.message = Some(msg.replace('\n', " "));
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
            bail!("{} を起動できません: {}", task.name, failed.join(", "));
        };
        // 表示するのはタスクの最初のウィンドウ
        self.current = first;
        if !failed.is_empty() {
            self.message = Some(format!("起動に失敗: {}", failed.join(", ")));
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
        if let Some(l) = self.launcher.as_mut() {
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
        }
    }

    // ---- マウス ----

    fn on_mouse(&mut self, ev: MouseEvent) {
        if !self.mouse {
            return;
        }
        let (w, h) = self.size;
        let pressed = matches!(ev.kind, MouseKind::Press(_));

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

    /// 終了したペインの案内の [r: restart] などをクリックしたら，そのキーを押したことにする．
    fn click_dead_pane(&mut self, id: PaneId, x: u16, y: u16) {
        let Some(r) = self.win().rect_of(id) else { return };
        let Some(pane) = self.panes.get(&id) else { return };
        if !pane.is_dead() {
            return;
        }
        // タイトル行のクリックは中身の外なので何もしない
        let (Some(row), Some(col)) = (y.checked_sub(r.y), x.checked_sub(r.x)) else { return };
        let line = pane.parser.lock().unwrap().screen().rows(0, r.w).nth(row as usize);
        let Some(line) = line else { return };
        let col = col as usize;
        for (label, key) in pane::EXIT_BUTTONS {
            if let Some(idx) = line.find(label) {
                let start = line[..idx].width();
                if (start..start + label.width()).contains(&col) {
                    self.dead_pane_key(id, key);
                    return;
                }
            }
        }
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
                Err(e) => self.message = Some(format!("再起動に失敗: {e}")),
            },
            b's' => {
                if let Err(e) = p.open_shell(id, self.tx.clone()) {
                    self.message = Some(format!("シェルを起動できません: {e}"));
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
                self.info = Some(if self.mouse { MOUSE_HINT.into() } else { "マウス操作: オフ（C-q m でオン）".into() });
            }
            Action::Quit => self.quit = true,
            Action::NewWindow => {
                let _ = self.new_window();
            }
            Action::OpenLauncher => self.launcher = Some(Launcher::default()),
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
                let limit = p.max_restarts.map_or(String::new(), |max| format!("/{max}"));
                pane.note(&format!("restarting in {}s ({}{limit})  q: cancel", p.restart_delay.as_secs_f64(), count + 1));
                self.restarts.insert(id, Instant::now() + p.restart_delay);
                self.restart_counts.insert(id, count + 1);
            } else {
                pane.note(&format!("restart limit reached ({count} times)"));
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
                Err(e) => self.message = Some(format!("再起動に失敗: {e}")),
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
            Err(e) => self.message = Some(format!("起動に失敗: {e}")),
        }
    }

    /// 待機中のペインに表示する内容．最後の行はボタン（WAIT_BUTTONS）．
    fn wait_lines(&self, w: &Wait) -> Vec<(String, Style)> {
        let head = Style { fg: Color::Yellow, bold: true, ..Style::default() };
        let ok = Style { fg: Color::Green, ..Style::default() };
        let wait = Style::default();
        let ng = Style { fg: Color::Red, bold: true, ..Style::default() };
        let button = Style { inverse: true, ..Style::default() };

        let mut lines = vec![(" waiting to start".to_string(), head)];
        for d in &w.deps {
            let st = deps::status(d, &self.runs, self.panes.contains_key(&d.pane));
            let (mark, style, note) = match st {
                DepStatus::Met => ("[ok]", ok, String::new()),
                DepStatus::Waiting => ("[..]", wait, String::new()),
                DepStatus::Blocked(why) => ("[NG]", ng, format!("  ({why})")),
            };
            lines.push((format!("   {mark} {}: {}{note}", d.name, d.cond.label()), style));
        }
        if !w.delay.is_zero() {
            let state = if w.ready_since.is_some() { "  (counting)" } else { "" };
            lines.push((format!("   then wait {}s{state}", w.delay.as_secs_f64()), wait));
        }
        lines.push((String::new(), wait));
        lines.push((format!(" {}  {}", WAIT_BUTTONS[0].0, WAIT_BUTTONS[1].0), button));
        lines
    }

    /// 待機中のペインのボタンがクリックされたか調べ，押されたキーを返す．
    fn wait_button_at(&self, id: PaneId, x: u16, y: u16) -> Option<u8> {
        let (w, r) = (self.waits.get(&id)?, self.win().rect_of(id)?);
        let lines = self.wait_lines(w);
        let (row, col) = (y.checked_sub(r.y)?, x.checked_sub(r.x)?);
        if row as usize != lines.len() - 1 {
            return None;
        }
        let line = &lines.last()?.0;
        let col = col as usize;
        WAIT_BUTTONS.iter().find_map(|(label, key)| {
            let idx = line.find(label)?;
            let start = line[..idx].width();
            (start..start + label.width()).contains(&col).then_some(*key)
        })
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
        let mut push = |items: &mut Vec<StatusItem>, text: String, style: Style, click: Option<Click>| {
            let width = text.width() as u16;
            items.push(StatusItem { x, text, style, click });
            x = x.saturating_add(width);
        };
        push(&mut items, " [shouji] ".into(), bar, None);
        for (i, wn) in self.windows.iter().enumerate() {
            let mark = if i == self.current { "*" } else { "" };
            let style = if i == self.current { cur } else { bar };
            push(&mut items, format!(" {i}:{}{mark} ", wn.name), style, Some(Click::Window(i)));
        }
        if self.input.in_prefix() {
            push(&mut items, " <PREFIX>".into(), bar, None);
        }

        let buttons = || -> Vec<(String, Style, Option<Click>)> {
            [
                (" +左右 ", Click::Split(Dir::Horizontal)),
                (" +上下 ", Click::Split(Dir::Vertical)),
                (" タスク ", Click::Launcher),
                (" ログ ", Click::Scroll),
                (" +ウィンドウ ", Click::NewWindow),
            ]
            .into_iter()
            .map(|(t, c)| (t.to_string(), btn, Some(c)))
            .collect()
        };
        let mut right: Vec<(String, Style, Option<Click>)> = Vec::new();
        if self.scrolling {
            let (off, len) = self.panes.get(&self.win().active).map_or((0, 0), |p| {
                let parser = p.parser.lock().unwrap();
                (parser.screen().scrollback(), parser.screen().scrollback_len())
            });
            push(&mut items, " ".into(), bar, None);
            push(&mut items, format!(" SCROLL {off}/{len}  k/j u/d g/G q:exit "), yellow, None);
            if self.mouse {
                right.push((" スクロール終了 ".into(), btn, Some(Click::ExitScroll)));
            }
        } else if let Some(msg) = &self.message {
            push(&mut items, " ".into(), bar, None);
            push(&mut items, format!(" {msg} "), err, None);
        } else {
            if let Some(info) = &self.info {
                push(&mut items, " ".into(), bar, None);
                push(&mut items, format!(" {info} "), note, None);
            }
            if self.mouse {
                right = buttons();
            } else {
                let help = "C-q Space launch | c new | n/p/0-9 win | %/\" split | o/arrows move | [ scroll | m mouse | x kill | d quit ";
                let short = "C-q Space: launcher ";
                if let Some(t) = [help, short].into_iter().find(|t| x as usize + t.width() < w as usize) {
                    right.push((t.to_string(), bar, None));
                }
            }
        }

        // 右寄せ．入りきらなければ後ろのボタンから省く
        let total = |r: &[(String, Style, Option<Click>)]| -> u16 {
            r.iter().map(|(t, _, _)| t.width() as u16 + 1).sum::<u16>().saturating_sub(1)
        };
        while !right.is_empty() && x as u32 + 1 + total(&right) as u32 > w as u32 {
            right.pop();
        }
        let mut rx = w.saturating_sub(total(&right));
        for (text, style, click) in right {
            let width = text.width() as u16;
            items.push(StatusItem { x: rx, text, style, click });
            rx = rx.saturating_add(width + 1);
        }
        items
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
            if let Some(w) = self.waits.get(id) {
                for (i, (line, style)) in self.wait_lines(w).iter().enumerate().take(r.h as usize) {
                    put_clipped(&mut frame, r.x, r.y + i as u16, line, *style, r.x + r.w);
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
            let state = if self.waits.contains_key(id) {
                " [waiting]"
            } else if self.restarts.contains_key(id) {
                " [restarting]"
            } else if pane.is_some_and(Pane::is_dead) {
                " [exited]"
            } else if pane.is_some_and(Pane::running_shell) {
                " (shell)"
            } else {
                ""
            };
            let name = program.map_or("shell", |p| p.name.as_str());
            let end = t.x + t.w;
            let mut x = t.x + put_clipped(&mut frame, t.x, t.y, &format!(" {name}{state} "), bar, end);
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

fn main() -> Result<()> {
    if std::env::var_os("SHOUJI").is_some() {
        bail!("shouji の中で shouji は起動できません");
    }

    // 設定は raw モードに入る前に読む（エラーを普通に表示するため）
    const USAGE: &str = "usage: shouji [-c CONFIG.yaml] [-t TASK.yaml]...\n\
        \n  -c, --config FILE  設定ファイル（省略時は ./shouji.yaml → ~/.config/shouji/config.yaml）\
        \n  -t, --task FILE    起動時に開くタスクファイル（複数指定可）";
    let mut config_path = None;
    let mut task_paths = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => config_path = Some(std::path::PathBuf::from(args.next().context(USAGE)?)),
            "-t" | "--task" => task_paths.push(args.next().context(USAGE)?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => bail!("不明な引数: {arg}\n{USAGE}"),
        }
    }
    let mut config = Config::load(config_path.as_deref())?;

    // -t のタスクはここで一度読んで，書式の誤りを raw モードに入る前に報告する．
    // メニューから開き直せるよう，タスク一覧にも加える．
    let cwd = std::env::current_dir()?;
    let mut startup = Vec::new();
    for path in &task_paths {
        let entry = TaskEntry::new(path, None, &cwd);
        entry.load()?;
        startup.push(config.add_task(entry));
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
