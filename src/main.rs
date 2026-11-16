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
mod ui;

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use unicode_width::UnicodeWidthChar;

use config::{Config, Restart, Task, TaskEntry};
use deps::{Dep, RunState, Wait};
use input::{Action, Direction, InputParser};
use help::Help;
use launcher::{Launcher, MenuResult, Selection};
use layout::{Dir, Node, PaneId, Rect};
use mouse::{Button, Kind as MouseKind, MouseEvent};
use pane::Pane;
use render::{Frame, Renderer, Style};

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

const MOUSE_HINT: &str = "Mouse: on (Shift+drag to select text, C-q m to turn off)";

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
    /// 各ペインのタイトル行（枠の上辺）．
    titles: Vec<(PaneId, Rect)>,
    /// 各ペインの枠を含めた領域．
    boxes: Vec<(PaneId, Rect)>,
    /// 選択中のペインだけを大きく表示しているか．
    zoom: bool,
}

impl Window {
    fn new(name: String, pane: PaneId) -> Self {
        Self { name, root: Some(Node::Leaf(pane)), active: pane, rects: Vec::new(), titles: Vec::new(), boxes: Vec::new(), zoom: false }
    }

    fn rect_of(&self, id: PaneId) -> Option<Rect> {
        self.rects.iter().find(|(i, _)| *i == id).map(|(_, r)| *r)
    }

    fn contains(&self, id: PaneId) -> bool {
        self.ids().contains(&id)
    }

    /// ウィンドウの全ペイン（表示順．大きく表示中で隠れているものも含む）．
    fn ids(&self) -> Vec<PaneId> {
        self.root.as_ref().map(Node::ids).unwrap_or_default()
    }

    /// レイアウトを計算し直し，サイズが変わったペインの PTY を合わせる．
    fn relayout(&mut self, area: Rect, panes: &mut HashMap<PaneId, Pane>) {
        let old = std::mem::take(&mut self.rects);
        self.titles.clear();
        self.boxes.clear();
        let mut full = Vec::new();
        if let Some(root) = &self.root {
            if self.zoom && self.contains(self.active) {
                full.push((self.active, area));
            } else {
                // 左右の区切り線の 1 列は枠どうしのすき間にする．
                // 上下の区切り線の 1 行は上のペインに足し，枠どうしをくっつける
                let mut seps = Vec::new();
                root.layout(area, &mut full, &mut seps);
                for sep in seps.iter().filter(|s| s.dir == Dir::Vertical) {
                    for (_, r) in full.iter_mut() {
                        if r.y + r.h == sep.y && r.x >= sep.x && r.x + r.w <= sep.x + sep.len {
                            r.h += 1;
                        }
                    }
                }
            }
        }
        // 各ペインを枠で囲み，上辺をタイトルにする（小さすぎれば枠なしでタイトル行だけ）
        for (id, r) in full {
            self.boxes.push((id, r));
            if r.w >= 4 && r.h >= 3 {
                self.titles.push((id, Rect { h: 1, ..r }));
                self.rects.push((id, Rect { x: r.x + 1, y: r.y + 1, w: r.w - 2, h: r.h - 2 }));
            } else if r.h >= 2 {
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

    /// 表示順で次（`back` なら前）のペインへ移る．
    fn focus_next(&mut self, back: bool) {
        let ids: Vec<PaneId> = self.rects.iter().map(|(id, _)| *id).collect();
        if let Some(i) = ids.iter().position(|id| *id == self.active) {
            let n = ids.len();
            self.active = ids[if back { (i + n - 1) % n } else { (i + 1) % n }];
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

/// 画面のクリックできる部分．
#[derive(Clone, Copy, Debug)]
enum Click {
    Window(usize),
    /// サイドバーのペイン（ウィンドウ, ペイン）．
    Pane(usize, PaneId),
    /// ペインの中のボタン（ペイン, 押したことにするキー）．
    PaneKey(PaneId, u8),
    /// 下部のヒント（押したことにするキー入力）．
    Keys(&'static [u8]),
    Split(Dir),
    NewWindow,
    Launcher,
    Help,
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
    /// 構成のサイドバーを出すか（C-q t で切り替え）．
    sidebar: bool,
    /// 終了の確認中か（C-q d か Quit ボタン）．
    confirm_quit: bool,
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
            sidebar: true,
            confirm_quit: false,
            message: None,
            info: None,
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

    /// 非表示のウィンドウも含めて，全ペインの配置とサイズを合わせ直す．
    fn relayout_all(&mut self) {
        let area = self.pane_area();
        for win in &mut self.windows {
            win.relayout(area, &mut self.panes);
        }
    }

    /// 大きく表示しているのをやめる（ペインを移るときなど）．
    fn unzoom(&mut self) {
        if self.win().zoom {
            self.win_mut().zoom = false;
            self.relayout_current();
        }
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
                    self.relayout_all();
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
        } else if self.confirm_quit {
            self.confirm_quit = false;
            if bytes == b"y" || bytes == b"Y" {
                self.quit = true;
            }
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

        // ボタン・タブ・ヒントなど: 表示中の部品で判定してからメッセージを消す
        let click = if ev.kind == MouseKind::Press(Button::Left) { self.click_at(ev.x, ev.y) } else { None };
        if pressed {
            self.message = None;
            self.info = None;
        }
        if let Some(c) = click {
            self.do_click(c);
            return;
        }
        let _ = (w, h);

        let Some(id) = self.pane_at(ev.x, ev.y) else { return };
        match ev.kind {
            MouseKind::Press(Button::Left) => self.focus_pane(id),
            MouseKind::WheelUp => self.wheel(id, true),
            MouseKind::WheelDown => self.wheel(id, false),
            _ => {}
        }
    }

    /// 表示中のウィンドウで (x, y) にあるペイン．
    fn pane_at(&self, x: u16, y: u16) -> Option<PaneId> {
        let win = self.win();
        win.boxes.iter().find(|(_, r)| r.contains(x, y)).map(|(id, _)| *id)
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
            self.unzoom();
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

    fn do_click(&mut self, click: Click) {
        match click {
            Click::Window(i) if i < self.windows.len() => {
                self.stop_scrolling();
                self.current = i;
            }
            Click::Window(_) => {}
            Click::Pane(i, id) if i < self.windows.len() => {
                self.stop_scrolling();
                self.current = i;
                self.focus_pane(id);
            }
            Click::Pane(..) => {}
            Click::PaneKey(id, key) => {
                self.focus_pane(id);
                if self.waits.contains_key(&id) {
                    self.waiting_key(id, key);
                } else {
                    self.dead_pane_key(id, key);
                }
            }
            Click::Keys(keys) => self.on_keys(keys),
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
            Action::Split(dir) => {
                self.unzoom();
                self.split(dir);
            }
            Action::FocusNext => {
                self.unzoom();
                self.win_mut().focus_next(false);
            }
            Action::Focus(d) => {
                self.unzoom();
                self.win_mut().focus_dir(d);
            }
            Action::Zoom => {
                let win = self.win_mut();
                win.zoom = !win.zoom;
                self.relayout_current();
            }
            Action::ToggleSidebar => {
                self.sidebar = !self.sidebar;
                self.relayout_all();
            }
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
            // 動いているプログラムが止まるので，確認してから終了する
            Action::Quit => self.confirm_quit = true,
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

        // 自動再起動（restart）．C-q q で閉じたときと autostart: false のペインは対象外
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
