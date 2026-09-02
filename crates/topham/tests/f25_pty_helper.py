# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# pty scaffolding for f25_piped_refusal_pty.rs -- see that file's module doc
# for what this proves and why a subprocess is the only way to prove it.
#
# `topham::tui::run` calls `ratatui::try_init`, which goes through crossterm.
# crossterm decides "is there a controlling terminal" by opening /dev/tty
# directly rather than checking whether *this process's* stdin/stdout are
# tty-shaped -- so the only way to tell it "stdin is a real tty, stdout is a
# redirected file" is to actually give the child process both: a real
# pseudo-terminal as its controlling terminal, and a real file (not a pipe
# `cargo test` already captured) on fd 1. `std::process::Command` alone can
# only hand a child pipes or inherited fds, never a *new* controlling
# terminal, which is why this lives in Python's `pty` module instead of pure
# Rust.
import os
import pty
import signal
import sys
import time

topham_bin = sys.argv[1]
out_path = sys.argv[2]
err_path = sys.argv[3]
config_home = sys.argv[4]
data_home = sys.argv[5]

pid, master_fd = pty.fork()
if pid == 0:
    # Child: fd 0 is the pty slave pty.fork() just made this process's
    # controlling terminal (a real tty). fd 1/2 are redirected to real files,
    # not pipes -- the exact "stdout redirected to a file/pipe but a
    # controlling tty present" scenario F25 describes.
    out_fd = os.open(out_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    err_fd = os.open(err_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    os.dup2(out_fd, 1)
    os.dup2(err_fd, 2)
    os.close(out_fd)
    os.close(err_fd)
    # Isolated homes, per the house rule against touching a real ~/.claude or
    # ~/.config -- topham reads XDG_CONFIG_HOME/XDG_DATA_HOME (env.rs), never
    # any login state, so this is the whole isolation this scenario needs.
    os.environ["XDG_CONFIG_HOME"] = config_home
    os.environ["XDG_DATA_HOME"] = data_home
    os.execv(topham_bin, [topham_bin])
    os._exit(127)  # unreachable unless exec itself failed

# Parent: let the child reach its event loop, then send the TUI's own quit
# key so a passing run (one that refused and exited immediately) and a
# failing run (one that opened the full screen) both terminate on their own
# rather than needing to be killed.
time.sleep(1.5)
try:
    os.write(master_fd, b"q")
except OSError:
    pass

deadline = time.time() + 8
status = None
while time.time() < deadline:
    try:
        done_pid, status = os.waitpid(pid, os.WNOHANG)
    except ChildProcessError:
        status = 0
        break
    if done_pid == pid:
        break
    time.sleep(0.1)
else:
    # Did not exit in time -- kill it so this helper (and the test waiting on
    # it) cannot hang the suite, and report that as a distinct outcome from
    # "exited but wrote bytes".
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
    print("TIMED_OUT")
    sys.exit(2)

os.close(master_fd)
exit_code = os.waitstatus_to_exitcode(status) if status is not None else -1
print(f"EXIT={exit_code}")
