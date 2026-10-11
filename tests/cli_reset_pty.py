#!/usr/bin/env python3
"""Real terminal IO and cross-process SQLite races; only throwaway test data is used."""
import atexit
import errno
import os
import pty
import select
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import termios
import time
import urllib.error
import urllib.parse
import urllib.request

BINARY, DATA_DIR, INITIAL_SESSION = sys.argv[1:]
OLD = 'pflaume-Nebel-7-Kupfer-Zitrone'
NEW = 'Quartz-lantern-42-Cobalt-Willow'
OTHER = 'Cobalt-Moon-53-cypress-Silver'
ENV = {**os.environ, 'DATA_DIR': DATA_DIR, 'HIBP_CHECK': 'false', 'RUST_LOG': 'error'}


def snapshot():
    with sqlite3.connect(os.path.join(DATA_DIR, 'stalias.db')) as conn:
        return conn.execute('SELECT key,value FROM settings ORDER BY key').fetchall(), conn.execute('SELECT * FROM sessions ORDER BY hash').fetchall()


TERMINALS = []


def cleanup_terminals():
    for terminal in TERMINALS:
        try:
            os.kill(terminal.pid, signal.SIGKILL)
            os.waitpid(terminal.pid, 0)
        except ProcessLookupError:
            pass
        os.close(terminal.fd)


atexit.register(cleanup_terminals)


class Terminal:
    def __init__(self, env=ENV):
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(BINARY, [BINARY, 'reset-admin-password'], env)
        self.pid, self.fd, self.output = pid, fd, b''
        TERMINALS.append(self)

    def read_until(self, marker):
        deadline = time.monotonic() + 15
        while marker not in self.output:
            assert time.monotonic() < deadline, 'terminal prompt timeout'
            if select.select([self.fd], [], [], 0.1)[0]:
                data = os.read(self.fd, 4096)
                assert data, 'terminal ended before prompt'
                self.output += data

    def enter(self, value):
        # Verify actual terminal echo flags, not just absence of typed text in a string.
        # rpassword prints the prompt before switching terminal modes. Wait for readiness.
        deadline = time.monotonic() + 2
        while termios.tcgetattr(self.fd)[3] & termios.ECHO:
            assert time.monotonic() < deadline, 'password echo enabled'
            time.sleep(0.001)
        os.write(self.fd, value.encode() + b'\n')

    def finish(self):
        deadline = time.monotonic() + 15
        while True:
            waited, status = os.waitpid(self.pid, os.WNOHANG)
            if select.select([self.fd], [], [], 0.1)[0]:
                try:
                    data = os.read(self.fd, 4096)
                    if data:
                        self.output += data
                except OSError as e:
                    if e.errno != errno.EIO:
                        raise
            if waited:
                os.close(self.fd)
                TERMINALS.remove(self)
                assert NEW.encode() not in self.output and OTHER.encode() not in self.output, 'secret echoed'
                return os.waitstatus_to_exitcode(status)
            if time.monotonic() > deadline:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
                os.close(self.fd)
                TERMINALS.remove(self)
                raise AssertionError('CLI completion timeout')


def reset(password, confirmation=None):
    terminal = Terminal()
    terminal.read_until(b'New admin password: ')
    terminal.enter(password)
    terminal.read_until(b'Repeat new password: ')
    terminal.enter(password if confirmation is None else confirmation)
    return terminal, terminal.finish()


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())


def request(path, fields=None, cookie=None):
    data = urllib.parse.urlencode(fields).encode() if fields is not None else None
    req = urllib.request.Request(BASE + path, data=data)
    if cookie:
        req.add_header('Cookie', cookie)
    try:
        response = opener.open(req, timeout=10)
    except urllib.error.HTTPError as error:
        response = error
    response.read()
    return response.code, response.headers


before = snapshot()
# Piped input is refused before inspecting the DB, even with a controlling terminal elsewhere.
result = subprocess.run([BINARY, 'reset-admin-password'], input=b'ignored\n', env=ENV, capture_output=True)
assert result.returncode != 0 and b'interactive terminal' in result.stderr
assert snapshot() == before
result = subprocess.run([BINARY, 'reset-admin-password', 'not-a-password-argument'], env=ENV, capture_output=True)
assert result.returncode != 0 and b'Usage:' in result.stderr
assert snapshot() == before

with tempfile.TemporaryDirectory(prefix='stalias-cli-empty-') as empty:
    terminal = Terminal({**ENV, 'DATA_DIR': empty})
    assert terminal.finish() != 0
    assert not os.path.exists(os.path.join(empty, 'stalias.db')), 'reset created a DB'
    with sqlite3.connect(os.path.join(empty, 'stalias.db')) as conn:
        conn.execute('CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL)')
    terminal = Terminal({**ENV, 'DATA_DIR': empty})
    assert terminal.finish() != 0
    assert b'No admin exists' in terminal.output

# Ctrl-C must leave hash and sessions unchanged.
terminal = Terminal()
terminal.read_until(b'New admin password: ')
os.write(terminal.fd, b'\x03')
assert terminal.finish() != 0
assert snapshot() == before
# Invalid policy and mismatched confirmation also leave state unchanged.
assert reset('short')[1] != 0
assert reset(NEW, OTHER)[1] != 0
assert snapshot() == before

with socket.socket() as socket_probe:
    socket_probe.bind(('127.0.0.1', 0))
    port = socket_probe.getsockname()[1]
BASE = f'http://127.0.0.1:{port}'
server = subprocess.Popen([BINARY], env={**ENV, 'PORT': str(port)}, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    deadline = time.monotonic() + 10
    while True:
        try:
            if request('/healthz')[0] == 200:
                break
        except urllib.error.URLError:
            pass
        assert time.monotonic() < deadline, 'test server did not start'
        time.sleep(0.05)
    initial_cookie = 'stalias_session=' + INITIAL_SESSION
    assert request('/settings/password', cookie=initial_cookie)[0] == 200
    # Server stays alive throughout the local reset and observes revocation immediately.
    terminal, status = reset(NEW)
    assert status == 0 and b'All devices have been logged out' in terminal.output
    assert snapshot()[1] == []
    assert request('/settings/password', cookie=initial_cookie)[0] == 303
    assert request('/login', {'password': OLD})[0] == 401
    status, headers = request('/login', {'password': NEW})
    assert status == 303
    cookie = headers['Set-Cookie'].split(';', 1)[0]
    # CLI read the old version, then a web change wins while the terminal is at its prompt.
    terminal = Terminal()
    terminal.read_until(b'New admin password: ')
    status, headers = request('/settings/password', {'current_password': NEW, 'password': OTHER, 'confirm': OTHER}, cookie)
    assert status == 303
    changed = snapshot()
    terminal.enter(NEW)
    terminal.read_until(b'Repeat new password: ')
    terminal.enter(NEW)
    assert terminal.finish() != 0
    assert b'changed concurrently' in terminal.output
    assert snapshot() == changed
    assert request('/login', {'password': OTHER})[0] == 303
    assert request('/healthz')[0] == 200
finally:
    server.terminate()
    try:
        server.wait(timeout=5)
    except subprocess.TimeoutExpired:
        server.kill()
        server.wait()
