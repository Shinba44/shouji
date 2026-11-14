# shouji

A terminal launcher: list your programs in a YAML file and start them all at once, each in its own pane.
It also works as a simple terminal multiplexer (splits, windows, scrollback).

## Usage

```sh
cargo run -- -t sample_config/task.yaml   # start with a task opened
cargo run -- -c sample_config/shouji.yaml # use a specific config (pick a task with C-q Space)
cargo run                                 # looks for ./shouji.yaml, then ~/.config/shouji/config.yaml
```

## Configuration

`shouji.yaml`: the tasks listed in the launcher

```yaml
tasks:
  - task.yaml
```

`task.yaml`: the programs started together

```yaml
name: dev
env: { APP_ENV: development }       # task < window < program (the narrower one wins)
windows:
  - name: app
    programs:
      - name: build
        command: cargo build
      - name: server
        command: cargo run
        after: [{ build: done }]     # wait until build exits successfully (or: started)
        restart: on-failure          # or { when, delay, max }
        log: logs/server.log
  - name: tools
    programs:
      - name: git-log
        command: git log --oneline
        autostart: false             # type the command into a shell and wait for Enter
```

Every option is described in `sample_config/task.yaml`.

## Keys (prefix: `Ctrl-q`)

| Key | Action |
|---|---|
| `C-q Space` | Open the task launcher |
| `C-q %` / `C-q "` | Split left/right, top/bottom |
| `C-q o`, `C-q h/j/k/l`, `C-q arrows` | Move between panes |
| `C-q c` / `C-q n` / `C-q p` / `C-q 0-9` | New window / switch windows |
| `C-q [` | Scroll mode (`k/j u/d g/G`, `q` to leave) |
| `C-q x` | Close the pane |
| `C-q m` | Toggle mouse support |
| `C-q d` | Quit |
| `C-q C-q` | Send a literal `Ctrl-q` |

In a pane whose program has exited, press `r` to restart, `s` for a shell, or `q` to close.
Mouse support (status bar buttons, clicking, wheel scrolling) is on at startup. While it is on, select text with `Shift`+drag.
