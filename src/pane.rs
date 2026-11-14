//! 1 つのペイン = PTY 上で動くシェル（またはプログラム）+ その仮想画面．
//!
//! プロセスが終了しても仮想画面（ログ）は残せる．再起動すると同じ画面の続きに出力される．

use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::Event;
use crate::config::Program;
use crate::logfile::LogFile;
use crate::layout::PaneId;

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// vt100 が扱わない問い合わせ（カーソル位置・端末種別）に応答する．
/// 応答しないと，fish など起動時に問い合わせるシェルが待たされる．
#[derive(Default)]
pub struct Replies {
    buf: Vec<u8>,
}

impl vt100::Callbacks for Replies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, first) {
            // DSR: カーソル位置の報告
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.buf.extend(format!("\x1b[{};{}R", row + 1, col + 1).bytes());
            }
            // DSR: 状態報告
            (None, 'n', 5) => self.buf.extend(b"\x1b[0n"),
            // DA1: VT102 として応答
            (None, 'c', 0) => self.buf.extend(b"\x1b[?6c"),
            _ => {}
        }
    }
}

pub type Parser = vt100::Parser<Replies>;

/// 実行中のプロセスと，その PTY．
struct Proc {
    writer: SharedWriter,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// 設定のプログラムとして起動したか（`s: shell` で開いたシェルなら false）．
    is_program: bool,
}

/// プロセスの終わり方．
pub struct Exit {
    /// 正常終了（終了コード 0）したか．
    pub success: bool,
    /// 終わり方の説明（`code 1`，`signal: Interrupt` など）．
    pub what: String,
    /// 設定のプログラムとして起動したプロセスだったか．
    pub was_program: bool,
}

pub struct Pane {
    pub parser: Arc<Mutex<Parser>>,
    /// 起動するプログラム（`None` ならログインシェル）．
    program: Option<Program>,
    /// 実行中のプロセス．終了後は `None`．
    proc: Option<Proc>,
    /// 何回目に起動したプロセスか．古い reader スレッドからの通知を見分ける．
    pub generation: u64,
    size: (u16, u16),
    /// ユーザーが閉じようとしている（終了しても残さない）．
    closing: bool,
    /// 出力を追記するログファイル（task.yaml の `log`）．
    log: Option<Arc<Mutex<LogFile>>>,
    /// ユーザーに伝えたい問題（ログファイルを開けないなど）．App が取り出して表示する．
    pub warning: Option<String>,
}

/// 終了時の案内に出すボタンと，それに対応するキー（クリックで押せる）．
pub const EXIT_BUTTONS: [(&str, u8); 3] = [("[r: restart]", b'r'), ("[s: shell]", b's'), ("[q: close]", b'q')];

/// スクロールバックとして保持する行数．
const SCROLLBACK: usize = 10_000;

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n.max(1)).unwrap()
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 }
}

impl Pane {
    /// `program` を（省略時はログインシェルを）起動する．
    pub fn spawn(id: PaneId, rows: u16, cols: u16, program: Option<&Program>, tx: Sender<Event>) -> Result<Self> {
        let mut pane = Self::idle(rows, cols, program);
        pane.start(id, tx, false)?;
        Ok(pane)
    }

    /// まだ起動しないペインを作る（`after` や `delay` で待つプログラム用）．
    /// 起動するときは [`Pane::launch`] を呼ぶ．
    pub fn idle(rows: u16, cols: u16, program: Option<&Program>) -> Self {
        let parser = vt100::Parser::new_with_callbacks(nz(rows), nz(cols), SCROLLBACK, Replies::default());
        let (log, warning) = match program.and_then(|p| p.log.as_deref()) {
            None => (None, None),
            Some(path) => match LogFile::open(path) {
                Ok(f) => (Some(Arc::new(Mutex::new(f))), None),
                Err(e) => (None, Some(format!("ログファイル {} を開けません: {e}", path.display()))),
            },
        };
        Self {
            parser: Arc::new(Mutex::new(parser)),
            program: program.cloned(),
            proc: None,
            generation: 0,
            size: (rows, cols),
            closing: false,
            log,
            warning,
        }
    }

    /// 待っていたプログラムを起動する．
    pub fn launch(&mut self, id: PaneId, tx: Sender<Event>) -> Result<()> {
        if self.proc.is_some() {
            return Ok(());
        }
        self.start(id, tx, false)
    }

    /// 新しいプロセスを起動し，出力を読み取るスレッドを立てる．仮想画面は引き継ぐ．
    /// `shell` が true なら，プログラムの代わりにシェルを（プログラムの cwd と env で）起動する．
    /// `autostart: false` のプログラムは，シェルを起動してコマンドを入力した状態にする．
    fn start(&mut self, id: PaneId, tx: Sender<Event>, shell: bool) -> Result<()> {
        let (rows, cols) = self.size;
        let pair = native_pty_system().openpty(pty_size(rows, cols))?;
        let program = self.program.as_ref();
        let typeahead = program.filter(|p| !shell && !p.autostart).map(|p| p.command.trim_end().to_string());

        let mut cmd = match program {
            Some(p) if !shell && typeahead.is_none() => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                let mut cmd = CommandBuilder::new(shell);
                cmd.args(["-c", &p.command]);
                cmd
            }
            _ => CommandBuilder::new_default_prog(),
        };
        cmd.env("TERM", "xterm-256color");
        cmd.env("SHOUJI", "1");
        if let Some(cwd) = program.and_then(|p| p.cwd.clone()).or_else(|| std::env::current_dir().ok()) {
            cmd.cwd(cwd);
        }
        for (k, v) in program.map(|p| &p.env).into_iter().flatten() {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd)?;
        // slave を閉じておかないと，子が終了しても reader が EOF にならない．
        drop(pair.slave);

        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer()?));
        let mut reader = pair.master.try_clone_reader()?;
        self.generation += 1;
        let generation = self.generation;
        let got_output = Arc::new(AtomicBool::new(false));

        if let Some(command) = typeahead {
            // シェルの準備（最初のプロンプト）を待ってからコマンドを入力する．
            // 早すぎると端末のエコーと二重に表示されてしまう．
            let writer = Arc::clone(&writer);
            let got_output = Arc::clone(&got_output);
            thread::spawn(move || {
                for _ in 0..500 {
                    if got_output.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                thread::sleep(Duration::from_millis(150));
                let mut w = writer.lock().unwrap();
                let _ = w.write_all(command.as_bytes());
                let _ = w.flush();
            });
        }

        {
            if let Some(log) = &self.log {
                let what = match program {
                    Some(p) if !shell => format!("started: {}", p.command.trim_end()),
                    _ => "started: shell".into(),
                };
                log.lock().unwrap().marker(&what);
            }
            let parser = Arc::clone(&self.parser);
            let writer = Arc::clone(&writer);
            let got_output = Arc::clone(&got_output);
            let log = self.log.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let replies = {
                        let mut p = parser.lock().unwrap();
                        p.process(&buf[..n]);
                        std::mem::take(&mut p.callbacks_mut().buf)
                    };
                    got_output.store(true, Ordering::Relaxed);
                    if let Some(log) = &log {
                        log.lock().unwrap().write(&buf[..n]);
                    }
                    if !replies.is_empty() {
                        let _ = writer.lock().unwrap().write_all(&replies);
                    }
                    if tx.send(Event::Output).is_err() {
                        return;
                    }
                }
                let _ = tx.send(Event::Exited(id, generation));
            });
        }

        self.proc = Some(Proc { writer, master: pair.master, child, is_program: !shell });
        self.closing = false;
        Ok(())
    }

    /// 実行中のプロセスの PID と，前面で動いているプロセス（グループ）の PID．
    /// 後者はシェルの中で実行したコマンドなど，Ctrl-C で止めたい相手になる．
    pub fn pids(&self) -> Option<(u32, Option<u32>)> {
        let proc = self.proc.as_ref()?;
        let pid = proc.child.process_id()?;
        let fg = proc.master.process_group_leader().and_then(|p| u32::try_from(p).ok());
        Some((pid, fg))
    }

    /// 設定のプログラム（シェルのペインなら `None`）．
    pub fn program(&self) -> Option<&Program> {
        self.program.as_ref()
    }

    /// プログラムのペインで，`s: shell` で開いたシェルが動いているか．
    pub fn running_shell(&self) -> bool {
        self.program.is_some() && self.proc.as_ref().is_some_and(|p| !p.is_program)
    }

    pub fn is_dead(&self) -> bool {
        self.proc.is_none()
    }

    /// 終了したときにペインを残すか．シェルは残さず，プログラムは設定に従う．
    pub fn keep_on_exit(&self) -> bool {
        !self.closing && self.program.as_ref().is_some_and(|p| p.keep_on_exit)
    }

    /// プロセスが終了したことを記録し，終わり方を返す．画面への案内は [`Pane::show_exit`] で出す．
    pub fn mark_exited(&mut self) -> Option<Exit> {
        let mut proc = self.proc.take()?;
        // PTY が閉じた直後はまだ終了していないことがあるので少し待つ
        let mut status = None;
        for _ in 0..20 {
            if let Ok(Some(s)) = proc.child.try_wait() {
                status = Some(s);
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let what = match &status {
            Some(s) => match s.signal() {
                Some(sig) => format!("signal: {sig}"),
                None => format!("code {}", s.exit_code()),
            },
            None => "status unknown".into(),
        };
        if let Some(log) = &self.log {
            log.lock().unwrap().marker(&format!("exited ({what})"));
        }
        Some(Exit { success: status.as_ref().is_some_and(|s| s.success()), what, was_program: proc.is_program })
    }

    /// 終了ステータスと，再起動・シェル・閉じるのボタンを画面に書き込む．
    pub fn show_exit(&mut self, what: &str) {
        let msg = format!("\x1b[0m\r\n\x1b[7m [exited ({what})]  [r: restart]  [s: shell]  [q: close]  C-q [: scroll \x1b[0m\r\n");
        self.parser.lock().unwrap().process(msg.as_bytes());
    }

    /// 画面に 1 行の案内を書き込む（自動再起動の待ちなど）．
    pub fn note(&mut self, text: &str) {
        let msg = format!("\x1b[0m\x1b[33m {text}\x1b[0m\r\n");
        self.parser.lock().unwrap().process(msg.as_bytes());
    }

    /// `C-q x` で閉じようとしているか（このときは自動再起動しない）．
    pub fn is_closing(&self) -> bool {
        self.closing
    }

    /// 終了したプロセスを，同じペイン（同じ画面）で起動し直す．
    pub fn restart(&mut self, id: PaneId, tx: Sender<Event>) -> Result<()> {
        if self.proc.is_some() {
            return Ok(());
        }
        self.parser.lock().unwrap().process(b"\x1b[0m\r\n");
        self.start(id, tx, false)
    }

    /// 終了したペインで，プログラムと同じ cwd・env のシェルを起動する．
    /// シェルを exit すると，また終了時の案内に戻る（r でプログラムを再起動できる）．
    pub fn open_shell(&mut self, id: PaneId, tx: Sender<Event>) -> Result<()> {
        if self.proc.is_some() {
            return Ok(());
        }
        self.parser.lock().unwrap().process(b"\x1b[0m\r\n");
        self.start(id, tx, true)
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if let Some(proc) = &self.proc {
            let mut w = proc.writer.lock().unwrap();
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.size = (rows, cols);
        if let Some(proc) = &self.proc {
            let _ = proc.master.resize(pty_size(rows, cols));
        }
        self.parser.lock().unwrap().set_size(nz(rows), nz(cols));
    }

    /// プロセスを止める．止まったあとペインは残さず閉じる．
    pub fn kill(&mut self) {
        self.closing = true;
        if let Some(proc) = &mut self.proc {
            let _ = proc.child.kill();
        }
    }

    /// 終了済みの子プロセスを回収する（ゾンビ化を防ぐ）．
    pub fn reap(&mut self) {
        if let Some(proc) = &mut self.proc {
            let _ = proc.child.try_wait();
        }
    }
}
