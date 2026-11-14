//! プログラムの出力をログファイルに追記する（task.yaml の `log`）．
//!
//! 色やカーソル移動などのエスケープシーケンスは取り除き，普通のテキストとして保存する．

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub struct LogFile {
    file: File,
    strip: Stripper,
    /// 最後に書いたのが改行か（区切りの行の前に改行を入れるため）．
    at_line_start: bool,
}

impl LogFile {
    /// 追記モードで開く．ディレクトリがなければ作る．
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file, strip: Stripper::default(), at_line_start: true })
    }

    /// プログラムの出力を書く．
    pub fn write(&mut self, bytes: &[u8]) {
        let text = self.strip.feed(bytes);
        if let Some(&last) = text.last() {
            self.at_line_start = last == b'\n';
            let _ = self.file.write_all(&text);
        }
    }

    /// `=== 日時 text ===` という区切りの行を書く．
    pub fn marker(&mut self, text: &str) {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let nl = if self.at_line_start { "" } else { "\n" };
        let _ = write!(self.file, "{nl}=== {now} {text} ===\n");
        self.at_line_start = true;
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Normal,
    /// ESC の直後．
    Esc,
    /// CSI（ESC [）の途中．
    Csi,
    /// OSC などの文字列（ESC ] ... BEL / ESC \）の途中．
    Str,
    /// 文字列の途中で ESC が来た（ESC \ で終わるか見る）．
    StrEsc,
}

/// エスケープシーケンスと制御文字を取り除く．読み込みの切れ目をまたいでも動く．
#[derive(Default)]
pub struct Stripper {
    state: State,
}

impl Stripper {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            self.state = match (self.state, b) {
                (State::Normal, 0x1b) => State::Esc,
                (State::Normal, b'\n' | b'\t') => {
                    out.push(b);
                    State::Normal
                }
                // \r（行頭へ戻る）やベルなどの制御文字は捨てる
                (State::Normal, 0x00..=0x1f | 0x7f) => State::Normal,
                (State::Normal, _) => {
                    out.push(b);
                    State::Normal
                }
                (State::Esc, b'[') => State::Csi,
                (State::Esc, b']' | b'P' | b'_' | b'^' | b'X') => State::Str,
                // ESC ( B など，中間バイトが続くもの
                (State::Esc, 0x20..=0x2f) => State::Esc,
                (State::Esc, _) => State::Normal,
                (State::Csi, 0x40..=0x7e) => State::Normal,
                (State::Csi, _) => State::Csi,
                (State::Str, 0x07) => State::Normal,
                (State::Str, 0x1b) => State::StrEsc,
                (State::Str, _) => State::Str,
                (State::StrEsc, b'\\') => State::Normal,
                (State::StrEsc, _) => State::Str,
            };
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escapes() {
        let mut s = Stripper::default();
        assert_eq!(s.feed(b"\x1b[1;31mred\x1b[0m ok\r\n"), b"red ok\n");
        assert_eq!(s.feed(b"\x1b]0;title\x07after\n"), b"after\n");
        assert_eq!(s.feed(b"\x1b]0;t\x1b\\x\x1b(By\n"), b"xy\n");
        assert_eq!(String::from_utf8(s.feed("日本語\n".as_bytes())).unwrap(), "日本語\n");
    }

    #[test]
    fn split_sequence() {
        let mut s = Stripper::default();
        assert_eq!(s.feed(b"a\x1b["), b"a");
        assert_eq!(s.feed(b"32mb"), b"b");
    }

    #[test]
    fn writes_file_with_markers() {
        let dir = std::env::temp_dir().join(format!("shouji-log-test-{}", std::process::id()));
        let path = dir.join("sub/out.log");
        let mut log = LogFile::open(&path).unwrap();
        log.marker("started: echo");
        log.write(b"\x1b[32mhello\x1b[0m");
        log.marker("exited (code 0)");
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("=== ") && lines[0].ends_with(" started: echo ==="));
        assert_eq!(lines[1], "hello");
        assert!(lines[2].ends_with(" exited (code 0) ==="));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
