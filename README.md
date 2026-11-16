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

What you type always goes to the selected pane. Most things can be done with the mouse
(tabs, the layout panel, the buttons at the bottom and in stopped panes); keyboard commands
start with `Ctrl-q`: press `Ctrl-q`, let go, then press the key.

| Key | Action |
|---|---|
| `C-q ?` | Help |
| `C-q Space` | Task launcher (`Enter` starts everything, `→` picks windows/programs) |
| `C-q o`, `C-q Tab`, `C-q arrows`, `C-q h/j/k/l` | Move between panes (or click a pane) |
| `C-q z` | Zoom the pane in / out |
| `C-q %` / `C-q "` | Split left/right, top/bottom |
| `C-q x` | Close the pane |
| `C-q c` / `C-q n` / `C-q p` / `C-q 1-9` | New window / switch windows (or click a tab) |
| `C-q [` | Scroll back (`k/j u/d g/G`, `q` to stop; the mouse wheel works too) |
| `C-q t` | Show / hide the layout panel |
| `C-q m` | Mouse on / off |
| `C-q d` | Quit (asks first) |
| `C-q C-q` | Send `Ctrl-q` to the program |

In a pane whose program has stopped, press `r` to run it again, `s` for a shell, or `x` to close it.
In a pane waiting to start, press `Enter` to start it now.
Each pane title shows the program's state (`Running`, `Waiting`, `Stopped`, `Finished`, `Shell`).
While the mouse is on, select text with `Shift`+drag. Colors need a terminal with 24-bit color.
