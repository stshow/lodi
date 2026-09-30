#!/usr/bin/env python3
"""Serve git smart HTTP (protocol v2) on loopback for gates and guest rows (LD-455).

    git-http-loopback.py serve --root DIR --port-file FILE --log FILE [--bind ADDR]
    git-http-loopback.py stop --pid PID [--timeout SECONDS]
    git-http-loopback.py --self-test

`serve` runs in the foreground: it binds ADDR (127.0.0.1 by default; any address that is not
loopback is refused, exit 2) on an ephemeral port, writes the port to FILE, and answers every
request by running `git http-backend` over the repositories below DIR, with GIT_PROTOCOL taken
from the request's Git-Protocol header. It appends one `METHOD PATH` line per request to the log
before answering, so the log's line count is the request count. One request is served at a time,
and its child is waited for, so no child outlives a request. SIGTERM stops it.

`stop` sends SIGTERM to PID and waits until that process, and every child it had, is gone.

http.server's CGIHTTPRequestHandler is not used: it is deprecated, and it drops the Git-Protocol
header, so git answers protocol v0, which Lodi's client refuses (measured: LD-455). Python standard
library only. The transport is plain HTTP on loopback, reached by Lodi through LODI_FETCH_REWRITE:
not TLS.
"""
import argparse
import http.server
import ipaddress
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import tempfile
import time
import urllib.request

BACKEND_TIMEOUT = 120


class Backend(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.0'

    def log_message(self, *_):
        pass

    def do_GET(self):
        self.answer()

    def do_POST(self):
        self.answer()

    def answer(self):
        with open(self.server.log, 'a', encoding='utf-8') as log:
            log.write(f'{self.command} {self.path}\n')
        if self.command == 'GET' and self.path.startswith('/assets/'):
            self.asset()
            return
        length = int(self.headers.get('Content-Length') or 0)
        body = self.rfile.read(length) if length else b''
        path, _, query = self.path.partition('?')
        env = {
            'PATH': os.environ.get('PATH', '/usr/local/bin:/usr/bin:/bin'),
            'GIT_PROJECT_ROOT': self.server.root,
            'GIT_HTTP_EXPORT_ALL': '1',
            'GIT_CONFIG_NOSYSTEM': '1',
            'HOME': self.server.root,
            'REQUEST_METHOD': self.command,
            'PATH_INFO': path,
            'QUERY_STRING': query,
            'CONTENT_TYPE': self.headers.get('Content-Type', ''),
            'CONTENT_LENGTH': str(len(body)),
            'REMOTE_ADDR': self.client_address[0],
            'GIT_PROTOCOL': self.headers.get('Git-Protocol', ''),
        }
        try:
            result = subprocess.run(['git', 'http-backend'], input=body, capture_output=True,
                                    env=env, timeout=BACKEND_TIMEOUT, check=False)
        except (OSError, subprocess.TimeoutExpired) as error:
            self.send_error(500, f'git http-backend: {error}')
            return
        head, sep, answer = result.stdout.partition(b'\r\n\r\n')
        if not sep:
            head, sep, answer = result.stdout.partition(b'\n\n')
        if not sep:
            self.send_error(500, 'git http-backend gave no CGI header')
            return
        status = 200
        headers = []
        for line in head.decode('latin-1').splitlines():
            name, _, value = line.partition(':')
            if name.lower() == 'status':
                status = int(value.split()[0])
            elif name:
                headers.append((name, value.strip()))
        self.send_response(status)
        for name, value in headers:
            if name.lower() != 'content-length':
                self.send_header(name, value)
        self.send_header('Content-Length', str(len(answer)))
        self.send_header('Connection', 'close')
        self.end_headers()
        self.wfile.write(answer)


    def asset(self):
        """A bounded regular file below the explicitly supplied assets directory; no links."""
        name = self.path[len('/assets/'):]
        if (not self.server.assets or name in ('', '.', '..')
                or any(c not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-'
                       for c in name)):
            self.send_error(404, 'not an asset name')
            return
        directory = file = None
        try:
            directory = os.open(self.server.assets, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            file = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
            facts = os.fstat(file)
            if not stat.S_ISREG(facts.st_mode) or facts.st_size > 16 * 1024 * 1024:
                self.send_error(404, 'not a bounded regular asset')
                return
            with os.fdopen(file, 'rb') as handle:
                file = None
                body = handle.read(16 * 1024 * 1024 + 1)
            if len(body) > 16 * 1024 * 1024:
                self.send_error(404, 'asset grew past its bound')
                return
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Connection', 'close')
            self.end_headers()
            self.wfile.write(body)
        except OSError:
            self.send_error(404, 'no regular asset')
        finally:
            for descriptor in (file, directory):
                if descriptor is not None:
                    os.close(descriptor)


def loopback(address):
    try:
        return ipaddress.ip_address(address).is_loopback
    except ValueError:
        return False


def serve(args):
    if not loopback(args.bind):
        print(f'git-http-loopback: {args.bind} is not a loopback address', file=sys.stderr)
        return 2
    server = http.server.HTTPServer((args.bind, 0), Backend)
    server.root = str(Path(args.root).resolve())
    server.log = args.log
    server.assets = args.assets
    Path(args.log).touch()
    temporary = f'{args.port_file}.{os.getpid()}'
    Path(temporary).write_text(f'{server.server_port}\n')
    os.replace(temporary, args.port_file)

    def stop(*_):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, stop)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


def children(pid):
    found = []
    for entry in Path('/proc').iterdir():
        if entry.name.isdigit():
            try:
                fields = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
            except OSError:
                continue
            if int(fields[1]) == pid:
                found.append(int(entry.name))
    return found


def alive(pid):
    try:
        state = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[0]
    except OSError:
        return False
    return state != 'Z'


def stop(pid, timeout):
    """SIGTERM `pid`, then wait until it and every child it had are gone. True when they are."""
    watched = [pid] + children(pid)
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not any(alive(p) for p in watched):
            return True
        time.sleep(0.05)
    return False


def wait_for(path, process, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if Path(path).exists():
            return Path(path).read_text().strip()
        if process.poll() is not None:
            return None
        time.sleep(0.05)
    return None


def self_test():
    """Offline: a scratch bare repository served on loopback answers protocol v2 to a v2 request
    and v0 without the header; a real git client lists it; the log counts every request; a
    non-loopback bind is refused; stop leaves no process."""
    checks = []

    def check(ok, what):
        checks.append(what)
        if not ok:
            raise AssertionError(what)

    script = str(Path(__file__).resolve())
    refused = subprocess.run([sys.executable, '-B', script, 'serve', '--bind', '0.0.0.0',
                              '--root', '/nonexistent', '--port-file', '/nonexistent/port',
                              '--log', '/nonexistent/log'], capture_output=True, check=False)
    check(refused.returncode == 2, 'a non-loopback bind is refused')
    identity = {'GIT_AUTHOR_NAME': 'gate', 'GIT_COMMITTER_NAME': 'gate',
                'GIT_AUTHOR_EMAIL': 'gate@users.noreply.github.com',
                'GIT_COMMITTER_EMAIL': 'gate@users.noreply.github.com',
                'GIT_AUTHOR_DATE': '2001-01-01T00:00:00+0000',
                'GIT_COMMITTER_DATE': '2001-01-01T00:00:00+0000',
                'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_NOSYSTEM': '1'}
    env = dict(os.environ, **identity)
    with tempfile.TemporaryDirectory(prefix='lodi-tmp.') as scratch:
        root = Path(scratch)
        work = root / 'work'

        def run(*argv):
            return subprocess.run(argv, check=True, capture_output=True, env=env)

        run('git', 'init', '-q', '-b', 'main', str(work))
        (work / 'host.toml').write_text('[host]\n')
        run('git', '-C', str(work), 'add', '-A')
        run('git', '-C', str(work), 'commit', '-qm', 'first')
        commit = run('git', '-C', str(work), 'rev-parse', 'HEAD').stdout.decode().strip()
        run('git', 'clone', '-q', '--bare', str(work), str(root / 'repo.git'))
        port_file, log = root / 'port', root / 'requests.log'
        server = subprocess.Popen([sys.executable, '-B', script, 'serve', '--root', str(root),
                                   '--port-file', str(port_file), '--log', str(log)],
                                  stdin=subprocess.DEVNULL)
        try:
            port = wait_for(port_file, server)
            check(port is not None and port.isdigit(), 'the port is written to the port file')
            url = f'http://127.0.0.1:{port}/repo.git'

            closes = []

            def ask(path, body=None, protocol='version=2'):
                headers = {'Git-Protocol': protocol} if protocol else {}
                if body is not None:
                    headers['Content-Type'] = 'application/x-git-upload-pack-request'
                request = urllib.request.Request(url + path, data=body, headers=headers)
                with urllib.request.urlopen(request, timeout=60) as reply:
                    closes.append(reply.headers.get('Connection'))
                    return reply.headers.get('Content-Type'), reply.read()

            kind, advert = ask('/info/refs?service=git-upload-pack')
            check(b'version 2' in advert and kind == 'application/x-git-upload-pack-advertisement',
                  'a Git-Protocol: version=2 request is answered in protocol v2')
            _, old = ask('/info/refs?service=git-upload-pack', protocol=None)
            check(b'# service=git-upload-pack' in old and b'version 2' not in old,
                  'without the header git answers protocol v0')
            ls = (b'0014command=ls-refs\n' b'0017object-format=sha1\n' b'0001'
                  b'0009peel\n' b'000csymrefs\n' b'0000')
            kind, refs = ask('/git-upload-pack', ls)
            check(commit.encode() in refs and kind == 'application/x-git-upload-pack-result',
                  'ls-refs over POST names the commit')
            # HTTP/1.0 closes after each answer; saying so keeps a pooling client from sending
            # its next request on a connection the server is closing (LD-402).
            check(closes == ['close'] * 3, 'every answer says Connection: close')
            listed = run('git', '-c', 'protocol.version=2', 'ls-remote', url).stdout.decode()
            check(commit in listed, 'a real git client lists the repository')
            lines = log.read_text().splitlines()
            check(lines[:3] == ['GET /repo.git/info/refs?service=git-upload-pack'] * 2
                  + ['POST /repo.git/git-upload-pack'] and len(lines) >= 4,
                  'the log counts every request')
        finally:
            gone = stop(server.pid, 30)
            server.wait(timeout=30)
        check(gone and not children(server.pid), 'stop leaves no process behind')
    print(f'git-http-loopback self-test: {len(checks)} checks passed')
    return 0


def main():
    if sys.argv[1:] == ['--self-test']:
        return self_test()
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    verbs = parser.add_subparsers(dest='verb', required=True)
    serving = verbs.add_parser('serve')
    serving.add_argument('--root', required=True)
    serving.add_argument('--port-file', required=True)
    serving.add_argument('--log', required=True)
    serving.add_argument('--assets', help='optional directory of pinned tool fixtures')
    serving.add_argument('--bind', default='127.0.0.1')
    stopping = verbs.add_parser('stop')
    stopping.add_argument('--pid', type=int, required=True)
    stopping.add_argument('--timeout', type=float, default=30)
    args = parser.parse_args()
    if args.verb == 'serve':
        return serve(args)
    return 0 if stop(args.pid, args.timeout) else 1


if __name__ == '__main__':
    sys.exit(main())
