# shouji

A terminal launcher: list your programs in a YAML file and start them all at once, each in its own pane.
It also works as a simple terminal multiplexer (splits, windows, scrollback).

## Usage

```sh
cargo run -- -t sample_config/task.yaml   # start with a task opened
cargo run -- -c sample_config/shouji.yaml # use a specific config (pick a task with C-q Space)
cargo run                                 # looks for ./shouji.yaml, then ~/.config/shouji/config.yaml
```

To start only part of a task, list window or program names after the task file:

```sh
cargo run -- -t sample_config/task.yaml app           # every program in the "app" window
cargo run -- -t sample_config/task.yaml build server  # only "build" and "server"
```

`after` dependencies on programs that were not selected are ignored.

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
        log: logs/server.log         # relative to cwd (or where shouji was started)
  - name: tools
    programs:
      - name: git-log
        command: git log --oneline
        autostart: false             # type the command into a shell and wait for Enter
```

`pre_command` runs in the same shell before `command` (and before the shell opened by `autostart: false` or `s`).
Like `env`, it can be set on the task, a window or a program; the narrowest one wins.

```yaml
name: ros
pre_command: . /opt/ros/noetic/setup.bash
programs:
  - name: roscore
    command: roscore
```

Every option is described in `sample_config/task.yaml`. `sample_config/ros.yaml` is a ROS example.

## Keys (prefix: `Ctrl-q`)

| Key | Action |
|---|---|
| `C-q ?` | Show all keys (also the `? Help` button in the status bar) |
| `C-q Space` | Open the task launcher (`Enter` starts everything, `→` picks windows/programs to start) |
| `C-q %` / `C-q "` | Split left/right, top/bottom |
| `C-q o`, `C-q h/j/k/l`, `C-q arrows` | Move between panes |
| `C-q c` / `C-q n` / `C-q p` / `C-q 0-9` | New window / switch windows |
| `C-q [` | Scroll mode (`k/j u/d g/G`, `q` to leave) |
| `C-q x` | Close the pane |
| `C-q m` | Toggle mouse support |
| `C-q d` | Quit |
| `C-q C-q` | Send a literal `Ctrl-q` |

Each pane title shows the program's state (`RUNNING`, `WAITING`, `DONE`, `FAILED`, ...).
In a pane whose program has stopped, press `r` to run it again, `s` for a shell, or `q` to close (or click the buttons at the bottom of the pane).
Mouse support (status bar buttons, clicking, wheel scrolling) is on at startup. While it is on, select text with `Shift`+drag.
