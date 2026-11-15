//! 設定ファイルの読み込み．
//!
//! `shouji.yaml` にはタスクファイルの一覧を書く．相対パスは shouji.yaml のある
//! ディレクトリから解決する．
//!
//! ```yaml
//! tasks:
//!   - task.yaml                 # パスだけでもよい（名前はファイル名から）
//!   - path: ~/work/api/task.yaml
//!     name: api
//! ```
//!
//! タスクファイル（例: `task.yaml`）には，一緒に起動するプログラムを書く．
//! 選ぶと新しいウィンドウを開き，各プログラムを分割したペインで起動する．
//!
//! ```yaml
//! name: dev                     # 省略時は shouji.yaml 側の名前
//! programs:
//!   - name: server
//!     command: cargo run
//!     cwd: ./server             # 相対パスはタスクファイルのディレクトリから
//!     env:
//!       RUST_LOG: debug
//!     pre_command: . ./env.sh   # command の前に同じシェルで実行（タスク・ウィンドウにも書ける）
//!     keep_on_exit: true        # 終了してもペインを残す（省略時 true）
//!     autostart: true           # false ならシェルにコマンドを入力した状態で待つ（Enter で実行）
//!   - name: client
//!     command: ./client
//!     after:                    # 起動を待つ相手（同じタスク内のプログラム名）
//!       - server: started       #   起動したら
//!       - build: done           #   正常終了（終了コード 0）したら
//!     delay: 2                  # さらに 2 秒待ってから起動
//!   - name: top
//!     command: top
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// `~` を展開し，相対パスなら `base` からのパスにする．
fn resolve(path: &str, base: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let p = match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) => home.join(rest.trim_start_matches('/')),
        _ => PathBuf::from(path),
    };
    if p.is_absolute() { p } else { base.join(p) }
}

fn read_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path).with_context(|| format!("{} を読めません", path.display()))?;
    serde_yaml_ng::from_str(&text).with_context(|| format!("{} の書式が不正です", path.display()))
}

// ---- shouji.yaml ----

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    tasks: Vec<RawTaskEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(
    untagged,
    expecting = "タスクはパスの文字列か，path と name（省略可）だけを持つ項目で書いてください"
)]
enum RawTaskEntry {
    Path(String),
    Full(RawTaskFull),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTaskFull {
    path: String,
    name: Option<String>,
}

#[derive(Debug, Default)]
pub struct Config {
    pub tasks: Vec<TaskEntry>,
}

/// shouji.yaml に書かれたタスクファイルへの参照．
#[derive(Debug, Clone)]
pub struct TaskEntry {
    pub name: String,
    /// 解決済みの絶対パス（または起動ディレクトリからのパス）．
    pub path: PathBuf,
}

impl Config {
    fn from_raw(raw: RawConfig, base: &Path) -> Self {
        let tasks = raw
            .tasks
            .into_iter()
            .map(|e| {
                let (path, name) = match e {
                    RawTaskEntry::Path(p) => (p, None),
                    RawTaskEntry::Full(RawTaskFull { path, name }) => (path, name),
                };
                TaskEntry::new(&path, name, base)
            })
            .collect();
        Self { tasks }
    }

    /// タスクを一覧に加え，その添字を返す．同じファイルがすでにあればその添字を返す．
    pub fn add_task(&mut self, entry: TaskEntry) -> usize {
        let key = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let new_key = key(&entry.path);
        if let Some(i) = self.tasks.iter().position(|t| key(&t.path) == new_key) {
            return i;
        }
        self.tasks.push(entry);
        self.tasks.len() - 1
    }

    fn load_file(path: &Path) -> Result<Self> {
        let raw = read_yaml(path)?;
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Ok(Self::from_raw(raw, &base))
    }

    /// `explicit` が指定されていればそれを，なければ
    /// `./shouji.yaml` → `~/.config/shouji/config.yaml` の順に探す．
    /// どれも無ければ空の設定を返す．
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        if let Some(path) = explicit {
            return Self::load_file(path);
        }
        let mut candidates = vec![PathBuf::from("shouji.yaml")];
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(home).join(".config/shouji/config.yaml"));
        }
        match candidates.iter().find(|p| p.is_file()) {
            Some(path) => Self::load_file(path),
            None => Ok(Self::default()),
        }
    }
}

// ---- task.yaml ----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTask {
    name: Option<String>,
    /// タスク内のすべてのプログラムに付ける環境変数．
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// タスク内のすべてのプログラムの command の前に実行するコマンド．
    pre_command: Option<String>,
    /// 1 つのウィンドウに並べる場合．
    programs: Option<Vec<RawProgram>>,
    /// ウィンドウに分ける場合．
    windows: Option<Vec<RawWindow>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWindow {
    name: String,
    /// このウィンドウのプログラムに付ける環境変数．
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// このウィンドウのプログラムの command の前に実行するコマンド．
    pre_command: Option<String>,
    programs: Vec<RawProgram>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProgram {
    name: String,
    command: String,
    pre_command: Option<String>,
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    keep_on_exit: bool,
    #[serde(default = "default_true")]
    autostart: bool,
    #[serde(default)]
    after: Vec<RawAfter>,
    delay: Option<f64>,
    restart: Option<RawRestart>,
    log: Option<String>,
}

/// `restart: on-failure` か，入れ子の `restart: { when, delay, max }`．
#[derive(Debug, Deserialize)]
#[serde(
    untagged,
    expecting = "restart は never / on-failure / always か，when・delay・max を持つ項目で書いてください"
)]
enum RawRestart {
    When(Restart),
    Full(RawRestartFull),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRestartFull {
    when: Restart,
    delay: Option<f64>,
    max: Option<u32>,
}

/// 終了したときに自動で再起動するか．
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    /// 再起動しない．
    #[default]
    Never,
    /// エラー（終了コード 0 以外，シグナル）で終了したときだけ．
    OnFailure,
    /// どう終わっても．
    Always,
}

/// `after` の 1 項目: `- build` か `- build: done`．
#[derive(Debug, Deserialize)]
#[serde(untagged, expecting = "after の項目は  - 名前  か  - 名前: started / done  で書いてください")]
enum RawAfter {
    Name(String),
    WithCond(BTreeMap<String, Cond>),
}

/// 待つ相手がどうなったら起動するか．
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cond {
    /// 起動したら．
    Started,
    /// 正常終了（終了コード 0）したら．
    Done,
}

impl Cond {
    pub fn label(self) -> &'static str {
        match self {
            Cond::Started => "started",
            Cond::Done => "done",
        }
    }
}

fn default_true() -> bool {
    true
}

/// 1 つのタスク = 一緒に起動するプログラムの集まり．
#[derive(Debug, Clone)]
pub struct Task {
    pub name: String,
    /// タスク内の全プログラム（after の添字はこの並び）．
    pub programs: Vec<Program>,
    /// ウィンドウごとの名前と，そこに並べるプログラムの添字．
    pub windows: Vec<TaskWindow>,
}

#[derive(Debug, Clone)]
pub struct TaskWindow {
    pub name: String,
    pub programs: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct Program {
    pub name: String,
    /// `$SHELL -c` で実行するコマンド．
    pub command: String,
    /// command の前に同じシェルで実行するコマンド（`source setup.bash` など）．
    /// 失敗したら command は実行しない．シェルを開くとき（autostart: false や s）にも実行する．
    pub pre_command: Option<String>,
    /// 作業ディレクトリ．省略時は shouji を起動したディレクトリ．
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    /// 終了してもペインを残す（ログを読めるように）．省略時は true．
    pub keep_on_exit: bool,
    /// すぐ起動するか．false ならシェルを開き，コマンドを入力した状態で待つ（Enter で実行）．
    pub autostart: bool,
    /// 起動前に待つ相手（同じタスク内のプログラムの添字）と条件．
    pub after: Vec<(usize, Cond)>,
    /// 待つ相手がそろってから，さらに待つ時間．
    pub delay: Duration,
    /// 自動再起動の方針．
    pub restart: Restart,
    /// 再起動までの待ち時間．
    pub restart_delay: Duration,
    /// 自動で再起動する回数の上限（`None` は無制限）．
    pub max_restarts: Option<u32>,
    /// 出力を追記するログファイル．相対パスは作業ディレクトリ（cwd，なければ shouji を起動した
    /// ディレクトリ）から解決する．
    pub log: Option<PathBuf>,
}

impl Program {
    /// 起動を待つ必要があるか．
    pub fn waits(&self) -> bool {
        !self.after.is_empty() || !self.delay.is_zero()
    }
}

impl TaskEntry {
    /// `path` は `~` 始まり可．相対パスなら `base` から解決する．`name` の省略時はファイル名．
    pub fn new(path: &str, name: Option<String>, base: &Path) -> Self {
        let path = resolve(path, base);
        let name = name.unwrap_or_else(|| {
            path.file_stem().map_or_else(|| "task".into(), |s| s.to_string_lossy().into_owned())
        });
        Self { name, path }
    }

    /// タスクファイルを読む（選ばれるたびに読むので，編集はすぐ反映される）．
    pub fn load(&self) -> Result<Task> {
        let raw: RawTask = read_yaml(&self.path)?;
        let base = self.path.parent().map(Path::to_path_buf).unwrap_or_default();
        Task::from_raw(raw, &self.name, &base).with_context(|| format!("{} の書式が不正です", self.path.display()))
    }
}

impl Task {
    fn from_raw(raw: RawTask, default_name: &str, base: &Path) -> Result<Self> {
        let task_name = raw.name.unwrap_or_else(|| default_name.into());
        // programs だけならタスク名のウィンドウ 1 つにまとめる
        let raw_windows = match (raw.programs, raw.windows) {
            (Some(programs), None) => vec![RawWindow { name: task_name.clone(), env: BTreeMap::new(), pre_command: None, programs }],
            (None, Some(windows)) => windows,
            (Some(_), Some(_)) => bail!("programs と windows はどちらか一方だけ書いてください"),
            (None, None) => bail!("programs か windows を書いてください"),
        };
        let mut windows = Vec::with_capacity(raw_windows.len());
        let mut all = Vec::new();
        for w in raw_windows {
            if w.programs.is_empty() {
                bail!("ウィンドウ {} の programs が空です", w.name);
            }
            let start = all.len();
            // env は タスク全体 → ウィンドウ → プログラム の順に重ね，狭い領域の値を優先する
            all.extend(w.programs.into_iter().map(|mut p| {
                let mut env = raw.env.clone();
                env.extend(w.env.clone());
                env.extend(std::mem::take(&mut p.env));
                p.env = env;
                // pre_command は狭い領域のものが丸ごと優先される（空文字列なら付けない）
                p.pre_command = p.pre_command.take().or_else(|| w.pre_command.clone()).or_else(|| raw.pre_command.clone());
                p
            }));
            windows.push(TaskWindow { name: w.name, programs: (start..all.len()).collect() });
        }
        let raw_programs = all;

        let names: Vec<&str> = raw_programs.iter().map(|p| p.name.as_str()).collect();
        let index_of = |me: &str, target: &str| -> Result<usize> {
            let mut found = names.iter().enumerate().filter(|(_, n)| **n == target).map(|(i, _)| i);
            match (found.next(), found.next()) {
                (Some(i), None) if target != me => Ok(i),
                (Some(_), None) => bail!("{me}: 自分自身を after に書くことはできません"),
                (Some(_), Some(_)) => bail!("{me}: after の {target} という名前のプログラムが複数あります"),
                (None, _) => bail!("{me}: after の {target} という名前のプログラムがありません"),
            }
        };

        let mut programs = Vec::with_capacity(raw_programs.len());
        for p in &raw_programs {
            let mut after = Vec::new();
            for a in &p.after {
                match a {
                    RawAfter::Name(n) => after.push((index_of(&p.name, n)?, Cond::Started)),
                    RawAfter::WithCond(map) => {
                        if map.len() != 1 {
                            bail!("{}: after の項目には 1 つずつ書いてください（例: - build: done）", p.name);
                        }
                        let (n, cond) = map.iter().next().unwrap();
                        after.push((index_of(&p.name, n)?, *cond));
                    }
                }
            }
            let secs = |key: &str, v: Option<f64>, default: f64| -> Result<Duration> {
                match v.unwrap_or(default) {
                    d if d.is_finite() && d >= 0.0 => Ok(Duration::from_secs_f64(d)),
                    d => bail!("{}: {key} は 0 以上の秒数で書いてください（{d}）", p.name),
                }
            };
            let delay = secs("delay", p.delay, 0.0)?;
            let (restart, restart_delay, max_restarts) = match &p.restart {
                None => (Restart::Never, None, None),
                Some(RawRestart::When(w)) => (*w, None, None),
                Some(RawRestart::Full(f)) => (f.when, f.delay, f.max),
            };
            let restart_delay = secs("restart.delay", restart_delay, 1.0)?;
            // log の相対パスは command と同じく作業ディレクトリから
            let cwd = p.cwd.as_ref().map(|c| resolve(c, base));
            let log_base = cwd.clone().or_else(|| std::env::current_dir().ok()).unwrap_or_default();
            programs.push(Program {
                name: p.name.clone(),
                command: p.command.clone(),
                pre_command: p.pre_command.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from),
                cwd,
                env: p.env.clone(),
                keep_on_exit: p.keep_on_exit,
                autostart: p.autostart,
                after,
                delay,
                restart,
                restart_delay,
                max_restarts,
                log: p.log.as_ref().map(|l| resolve(l, &log_base)),
            });
        }
        check_cycle(&programs)?;
        Ok(Self { name: task_name, programs, windows })
    }
}

impl Task {
    /// 名前（ウィンドウ名かプログラム名）で起動するプログラムを絞り込む．
    /// ウィンドウ名ならその中のプログラムすべて，プログラム名ならそれだけ．
    /// 選ばなかったプログラムを待つ after は取り除く（待ち続けないように）．
    pub fn select(&mut self, names: &[String]) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let mut selected = vec![false; self.programs.len()];
        for name in names {
            let window = self.windows.iter().find(|w| w.name == *name);
            let program = self.programs.iter().position(|p| p.name == *name);
            match (window, program) {
                (Some(w), None) => w.programs.iter().for_each(|&k| selected[k] = true),
                (None, Some(k)) => selected[k] = true,
                (Some(_), Some(_)) => bail!("{name} はウィンドウ名とプログラム名の両方にあるため区別できません"),
                (None, None) => {
                    let windows: Vec<&str> = self.windows.iter().map(|w| w.name.as_str()).collect();
                    let programs: Vec<&str> = self.programs.iter().map(|p| p.name.as_str()).collect();
                    bail!(
                        "{name} というウィンドウもプログラムもありません（ウィンドウ: {} / プログラム: {}）",
                        windows.join(", "),
                        programs.join(", ")
                    );
                }
            }
        }
        self.select_mask(&selected);
        Ok(())
    }

    /// プログラムごとに選ぶか（`selected[k]`）で絞り込む．
    /// 選ばなかったプログラムを待つ after は取り除く（待ち続けないように）．
    pub fn select_mask(&mut self, selected: &[bool]) {
        let selected = |k: usize| selected.get(k).copied().unwrap_or(false);
        for w in &mut self.windows {
            w.programs.retain(|&k| selected(k));
        }
        self.windows.retain(|w| !w.programs.is_empty());
        for (k, p) in self.programs.iter_mut().enumerate() {
            if selected(k) {
                p.after.retain(|&(dep, _)| selected(dep));
            }
        }
    }
}

/// after が循環していないか（a → b → a など）を調べる．
fn check_cycle(programs: &[Program]) -> Result<()> {
    // 0: 未訪問，1: 探索中，2: 済み
    fn visit(i: usize, programs: &[Program], mark: &mut [u8], path: &mut Vec<usize>) -> Result<()> {
        match mark[i] {
            2 => return Ok(()),
            1 => {
                let start = path.iter().position(|&p| p == i).unwrap_or(0);
                let mut names: Vec<&str> = path[start..].iter().map(|&p| programs[p].name.as_str()).collect();
                names.push(&programs[i].name);
                bail!("after が循環しています: {}", names.join(" → "));
            }
            _ => {}
        }
        mark[i] = 1;
        path.push(i);
        for &(dep, _) in &programs[i].after {
            visit(dep, programs, mark, path)?;
        }
        path.pop();
        mark[i] = 2;
        Ok(())
    }
    let mut mark = vec![0u8; programs.len()];
    for i in 0..programs.len() {
        visit(i, programs, &mut mark, &mut Vec::new())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config() {
        let raw: RawConfig =
            serde_yaml_ng::from_str("tasks:\n  - task.yaml\n  - path: /abs/api.yaml\n    name: api\n").unwrap();
        let cfg = Config::from_raw(raw, Path::new("/proj"));
        assert_eq!(cfg.tasks.len(), 2);
        assert_eq!(cfg.tasks[0].name, "task");
        assert_eq!(cfg.tasks[0].path, PathBuf::from("/proj/task.yaml"));
        assert_eq!(cfg.tasks[1].name, "api");
        assert_eq!(cfg.tasks[1].path, PathBuf::from("/abs/api.yaml"));
    }

    #[test]
    fn parse_task() {
        let raw: RawTask = serde_yaml_ng::from_str(
            "programs:\n  - name: top\n    command: top\n  - name: srv\n    command: python3 -m http.server\n    cwd: ./srv\n    env: {PORT: '8000'}\n",
        )
        .unwrap();
        let task = Task::from_raw(raw, "dev", Path::new("/proj")).unwrap();
        assert_eq!(task.name, "dev");
        assert_eq!(task.programs.len(), 2);
        assert_eq!(task.programs[1].env["PORT"], "8000");
        assert_eq!(task.programs[1].cwd, Some(PathBuf::from("/proj/./srv")));
        assert_eq!(task.programs[0].cwd, None);
        assert!(task.programs[0].keep_on_exit);
        assert!(task.programs[0].autostart);
    }

    fn task(yaml: &str) -> Result<Task> {
        Task::from_raw(serde_yaml_ng::from_str(yaml)?, "t", Path::new("/proj"))
    }

    #[test]
    fn parse_after_and_delay() {
        let t = task(
            "programs:\n  - name: build\n    command: make\n  - name: srv\n    command: ./srv\n  - name: cli\n    command: ./cli\n    after:\n      - build: done\n      - srv\n    delay: 1.5\n",
        )
        .unwrap();
        assert_eq!(t.programs[2].after, vec![(0, Cond::Done), (1, Cond::Started)]);
        assert_eq!(t.programs[2].delay, Duration::from_millis(1500));
        assert!(t.programs[2].waits() && !t.programs[0].waits());
    }

    #[test]
    fn parse_restart_and_log() {
        let t = task(
            "programs:\n  - name: a\n    command: x\n  - name: b\n    command: y\n    restart:\n      when: on-failure\n      delay: 0.5\n      max: 3\n    log: logs/b.log\n  - name: c\n    command: z\n    restart: always\n    cwd: ./srv\n    log: c.log\n  - name: d\n    command: w\n    log: /var/log/d.log\n",
        )
        .unwrap();
        let (a, b) = (&t.programs[0], &t.programs[1]);
        assert_eq!((a.restart, a.restart_delay, a.max_restarts, &a.log), (Restart::Never, Duration::from_secs(1), None, &None));
        assert_eq!(b.restart, Restart::OnFailure);
        assert_eq!(b.restart_delay, Duration::from_millis(500));
        assert_eq!(b.max_restarts, Some(3));
        // log の相対パスは cwd から．cwd がなければ shouji を起動したディレクトリから
        assert_eq!(b.log, Some(std::env::current_dir().unwrap().join("logs/b.log")));
        assert_eq!(t.programs[2].log, Some(PathBuf::from("/proj/./srv/c.log")));
        assert_eq!(t.programs[3].log, Some(PathBuf::from("/var/log/d.log")));
        // 1 行の書き方では delay と max は省略時の値
        let c = &t.programs[2];
        assert_eq!((c.restart, c.restart_delay, c.max_restarts), (Restart::Always, Duration::from_secs(1), None));

        let e = |y: &str| format!("{:#}", task(y).unwrap_err());
        assert!(e("programs:\n  - name: a\n    command: x\n    restart: sometimes\n").contains("restart は"));
        // 入れ子の中の不明な項目や，以前の書き方はエラーにする
        assert!(e("programs:\n  - name: a\n    command: x\n    restart:\n      when: always\n      retries: 3\n").contains("restart は"));
        assert!(e("programs:\n  - name: a\n    command: x\n    restart: always\n    max_restarts: 3\n").contains("max_restarts"));
        assert!(e("programs:\n  - name: a\n    command: x\n    restart:\n      when: always\n      delay: -1\n").contains("restart.delay は 0 以上"));
    }

    #[test]
    fn env_layers() {
        let t = task(
            "env: {A: task, B: task, C: task}\nwindows:\n  - name: w1\n    env: {B: win, C: win}\n    programs:\n      - name: p1\n        command: x\n        env: {C: prog}\n      - name: p2\n        command: y\n  - name: w2\n    programs:\n      - name: p3\n        command: z\n",
        )
        .unwrap();
        let env = |i: usize| t.programs[i].env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ");
        // 狭い領域の値が優先される: プログラム > ウィンドウ > タスク
        assert_eq!(env(0), "A=task B=win C=prog");
        assert_eq!(env(1), "A=task B=win C=win");
        assert_eq!(env(2), "A=task B=task C=task");

        // programs だけのタスクでも，タスク全体の env が効く
        let t = task("env: {A: task}\nprograms:\n  - name: p\n    command: x\n    env: {B: prog}\n").unwrap();
        assert_eq!(t.programs[0].env.len(), 2);
    }

    #[test]
    fn pre_command_layers() {
        let t = task(
            "pre_command: . task.sh\nwindows:\n  - name: w1\n    pre_command: . win.sh\n    programs:\n      - name: p1\n        command: x\n        pre_command: '  . prog.sh\n'\n      - name: p2\n        command: y\n      - name: p3\n        command: y\n        pre_command: ''\n  - name: w2\n    programs:\n      - name: p4\n        command: z\n",
        )
        .unwrap();
        let pre = |i: usize| t.programs[i].pre_command.as_deref();
        // 狭い領域のものが丸ごと優先される．空文字列なら付けない
        assert_eq!(pre(0), Some(". prog.sh"));
        assert_eq!(pre(1), Some(". win.sh"));
        assert_eq!(pre(2), None);
        assert_eq!(pre(3), Some(". task.sh"));
    }

    #[test]
    fn select_by_name() {
        let yaml = "windows:\n  - name: app\n    programs:\n      - name: build\n        command: make\n      - name: server\n        command: ./srv\n        after: [{build: done}]\n  - name: tools\n    programs:\n      - name: log\n        command: git log\n";
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let shape = |t: &Task| t.windows.iter().map(|w| format!("{}:{:?}", w.name, w.programs)).collect::<Vec<_>>();

        // ウィンドウ名ならその中すべて
        let mut t = task(yaml).unwrap();
        t.select(&names(&["app"])).unwrap();
        assert_eq!(shape(&t), ["app:[0, 1]"]);
        assert_eq!(t.programs[1].after, vec![(0, Cond::Done)]);

        // プログラム名ならそれだけ．選ばなかった build を待つ after は外れる
        let mut t = task(yaml).unwrap();
        t.select(&names(&["server", "log"])).unwrap();
        assert_eq!(shape(&t), ["app:[1]", "tools:[2]"]);
        assert!(t.programs[1].after.is_empty() && !t.programs[1].waits());

        // 名前なしなら全部
        let mut t = task(yaml).unwrap();
        t.select(&[]).unwrap();
        assert_eq!(shape(&t), ["app:[0, 1]", "tools:[2]"]);

        let mut t = task(yaml).unwrap();
        let e = format!("{:#}", t.select(&names(&["nope"])).unwrap_err());
        assert!(e.contains("nope というウィンドウもプログラムもありません") && e.contains("ウィンドウ: app, tools"));
        let mut t = task("windows:\n  - name: x\n    programs:\n      - name: x\n        command: y\n").unwrap();
        assert!(format!("{:#}", t.select(&names(&["x"])).unwrap_err()).contains("区別できません"));
    }

    #[test]
    fn parse_windows() {
        let t = task(
            "name: dev\nwindows:\n  - name: app\n    programs:\n      - name: build\n        command: make\n      - name: srv\n        command: ./srv\n        after: [build]\n  - name: mon\n    programs:\n      - name: logs\n        command: tail -f log\n        after: [srv]\n",
        )
        .unwrap();
        assert_eq!(t.windows.len(), 2);
        assert_eq!((t.windows[0].name.as_str(), t.windows[0].programs.clone()), ("app", vec![0, 1]));
        assert_eq!((t.windows[1].name.as_str(), t.windows[1].programs.clone()), ("mon", vec![2]));
        // 別のウィンドウのプログラムも after で待てる
        assert_eq!(t.programs[2].after, vec![(1, Cond::Started)]);

        // programs だけなら，タスク名のウィンドウ 1 つ
        let t = task("programs:\n  - name: a\n    command: x\n").unwrap();
        assert_eq!(t.windows.len(), 1);
        assert_eq!(t.windows[0].name, "t");

        let e = |y: &str| format!("{:#}", task(y).unwrap_err());
        assert!(e("programs:\n  - name: a\n    command: x\nwindows: []\n").contains("どちらか一方"));
        assert!(e("name: x\n").contains("programs か windows"));
        assert!(e("windows:\n  - name: w\n    programs: []\n").contains("ウィンドウ w の programs が空"));
    }

    #[test]
    fn after_errors() {
        let e = |y: &str| format!("{:#}", task(y).unwrap_err());
        assert!(e("programs:\n  - name: a\n    command: x\n    after: [b]\n").contains("b という名前のプログラムがありません"));
        assert!(e("programs:\n  - name: a\n    command: x\n    after: [a]\n").contains("自分自身"));
        assert!(e("programs:\n  - name: a\n    command: x\n    after: [b]\n  - name: b\n    command: y\n    after: [a]\n")
            .contains("循環しています: a → b → a"));
        assert!(e("programs:\n  - name: a\n    command: x\n  - name: b\n    command: y\n    after:\n      - a: finished\n").contains("after の項目は"));
        assert!(e("programs:\n  - name: a\n    command: x\n    delay: -1\n").contains("delay は 0 以上"));
    }

    #[test]
    fn unknown_field_is_error() {
        assert!(serde_yaml_ng::from_str::<RawTask>("programs:\n  - name: a\n    comand: x\n").is_err());
        assert!(serde_yaml_ng::from_str::<RawConfig>("programs: []\n").is_err());
        // 不明な項目（以前の autostart など）は無視せずエラーにする
        assert!(serde_yaml_ng::from_str::<RawConfig>("tasks:\n  - path: a.yaml\n    autostart: true\n").is_err());
    }
}
