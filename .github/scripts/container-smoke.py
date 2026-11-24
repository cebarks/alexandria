#!/usr/bin/env python3
"""Exercise the release image over its private, loopback-published HTTP leg."""

from contextlib import ExitStack, closing
from html import unescape
from http.client import HTTPConnection, HTTPException
from http.cookies import CookieError, SimpleCookie
import json
from pathlib import Path
import re
import secrets
import subprocess
import sys
from tempfile import TemporaryDirectory
import time
from urllib.parse import urlencode

HOST = "alexandria-smoke.test"
ORIGIN = f"https://{HOST}"
LOGIN = "ci-smoke"
PROTOCOL = "2025-03-26"
MAX_BODY = 256 * 1024
CONFIG = f'''[server]
public_origin = "{ORIGIN}"
allowed_hosts = ["{HOST}"]
allowed_origins = ["{ORIGIN}"]
'''
BOOTSTRAP = '''
umask 077
IFS= read -r password
file=$(mktemp /tmp/alexandria-owner.XXXXXX)
printf '%s\\n' "$password" > "$file"
unset password
exec /usr/local/bin/alexandria bootstrap-owner --login "$1" --password-file "$file"
'''


class SmokeFailure(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise SmokeFailure(message)


def request(port, method, path, body=None, headers=None):
    with closing(HTTPConnection("127.0.0.1", port, timeout=5)) as client:
        client.request(method, path, body, {"Host": HOST, **(headers or {})})
        response = client.getresponse()
        if response.headers.get_content_type() == "text/event-stream":
            # Ignore empty priming events; only the first actual initialize result matters.
            remaining = MAX_BODY
            while remaining > 0:
                line = response.readline(remaining + 1)
                if not line:
                    break
                remaining -= len(line)
                if remaining >= 0 and line.startswith(b"data:") and line[5:].strip():
                    return response.status, response.headers, line[5:].strip()
            raise SmokeFailure("initialize did not return a bounded SSE data event")
        data = response.read(MAX_BODY + 1)
        require(len(data) <= MAX_BODY, f"oversized response from {path}")
        return response.status, response.headers, data


def wait_ready(port, running):
    deadline = time.monotonic() + 300
    while time.monotonic() < deadline:
        require(running(), "container exited before /health was ready")
        try:
            status, _, body = request(port, "GET", "/health")
        except (OSError, HTTPException):
            pass
        else:
            if status == 200:
                health = json.loads(body)
                require(isinstance(health, dict) and health.get("status") == "ok"
                        and health.get("version"), "invalid /health payload")
                return
        time.sleep(2)
    raise SmokeFailure("/health did not become ready within 300s")


def form_value(body, pattern, label):
    match = re.search(pattern, body.decode())
    require(match is not None, f"missing {label}")
    return unescape(match[1])


def issue_token(port, password):
    status, _, page = request(port, "GET", "/debug/login")
    require(status == 200, f"login form returned HTTP {status}")
    preauth = form_value(page, r'name="csrf" value="([^"]+)"', "login CSRF value")
    headers = {"Content-Type": "application/x-www-form-urlencoded", "Origin": ORIGIN}
    form = urlencode({"login": LOGIN, "password": password, "csrf": preauth})
    status, response, _ = request(port, "POST", "/debug/login", form, headers)
    require(status == 303 and response.get("Location") == "/debug", f"owner login returned HTTP {status}")
    cookies = SimpleCookie()
    for header in response.get_all("Set-Cookie", []):
        cookies.load(header)
    session = cookies.get("alexandria_session")
    csrf = cookies.get("alexandria_csrf")
    require(session and csrf and session["secure"] and csrf["secure"], "missing hosted Secure cookies")
    # The test client plays the proxy's private HTTP leg. It is not a browser/TLS smoke.
    headers["Cookie"] = f"{session.key}={session.coded_value}; {csrf.key}={csrf.coded_value}"
    form = urlencode({"name": "container smoke", "csrf": csrf.value})
    status, _, page = request(port, "POST", "/debug/account/tokens", form, headers)
    require(status == 200, f"token issuance returned HTTP {status}")
    return form_value(page, r'data-one-time-secret="([^"]+)"', "issued API token")


def initialize(port, token=None):
    headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    return request(port, "POST", "/mcp", json.dumps({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": PROTOCOL, "capabilities": {},
                   "clientInfo": {"name": "container-smoke", "version": "0"}},
    }), headers)


def check_admission(port, password):
    status, _, _ = request(port, "GET", "/debug")
    require(status == 403, f"anonymous /debug returned HTTP {status}, expected 403")
    status, headers, _ = initialize(port)
    require(status == 401 and not headers.get("Mcp-Session-Id"), f"anonymous initialize returned HTTP {status} or issued a session")
    print("Anonymous debug and MCP requests refused", flush=True)
    status, headers, body = initialize(port, issue_token(port, password))
    require(status == 200 and headers.get("Mcp-Session-Id"), f"authenticated initialize returned HTTP {status} or no session")
    reply = json.loads(body)
    require(isinstance(reply, dict) and reply.get("jsonrpc") == "2.0"
            and reply.get("id") == 1, "invalid initialize envelope")
    result = reply.get("result")
    require(isinstance(result, dict) and result.get("protocolVersion") == PROTOCOL,
            "initialize did not negotiate the requested protocol")
    info = result.get("serverInfo")
    require(isinstance(info, dict) and info.get("name") and result.get("instructions"),
            "initialize omitted server info or instructions")
    print("Authenticated MCP initialize passed", flush=True)


def docker(*args, input_data=None, timeout=30):
    result = subprocess.run(["docker", *args], input=input_data, capture_output=True,
                            text=True, timeout=timeout, check=True)
    return result.stdout.strip()


def smoke(image):
    password = secrets.token_urlsafe(32)
    with TemporaryDirectory(prefix="alexandria-smoke-") as directory, ExitStack() as owned:
        config = Path(directory) / "config.toml"
        config.write_text(CONFIG, encoding="utf-8")
        # Docker mounts this non-secret file for the image's unprivileged UID.
        config.chmod(0o644)
        volume = docker("volume", "create")
        owned.callback(docker, "volume", "rm", volume)
        mount = ("--volume", f"{volume}:/data")
        bootstrap = docker(
            "create", "--pull=never", "--interactive", "--network=none", *mount,
            "--tmpfs", "/tmp:nodev,nosuid,noexec,mode=1777", "--entrypoint=/bin/sh",
            image, "-eu", "-c", BOOTSTRAP, "bootstrap", LOGIN,
        )
        owned.callback(docker, "rm", "-f", bootstrap)
        docker("start", "--attach", "--interactive", bootstrap,
               input_data=password + "\n", timeout=120)
        require(docker("inspect", "--format", "{{.State.ExitCode}}", bootstrap) == "0", "owner bootstrap failed")
        network = docker("network", "create", f"alexandria-smoke-{secrets.token_hex(6)}")
        owned.callback(docker, "network", "rm", network)
        server = docker(
            "create", "--pull=never", "--network", network, *mount,
            "--publish", "127.0.0.1::3000",
            "--mount", f"type=bind,source={config},target=/run/alexandria-ci.toml,readonly",
            "--env", "ALEXANDRIA_CONFIG=/run/alexandria-ci.toml", image,
        )
        owned.callback(docker, "rm", "-f", server)
        docker("start", server)
        host, _, port = docker("port", server, "3000/tcp").partition(":")
        require(host == "127.0.0.1" and port.isdecimal(), "unexpected published smoke endpoint")
        try:
            wait_ready(int(port), lambda: docker("inspect", "--format", "{{.State.Running}}", server) == "true")
        except (SmokeFailure, ValueError):
            # Only print boot logs before any authenticated request or token issuance.
            subprocess.run(["docker", "logs", "--tail", "30", server], timeout=10, check=False)
            raise
        print("Release image is healthy", flush=True)
        check_admission(int(port), password)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: container-smoke.py IMAGE")
    try:
        smoke(sys.argv[1])
    except SmokeFailure as error:
        raise SystemExit(str(error)) from None
    except subprocess.CalledProcessError as error:
        raise SystemExit(f"Docker {error.cmd[1]} exited with status {error.returncode}") from None
    except (subprocess.SubprocessError, OSError, CookieError, HTTPException, ValueError) as error:
        # Do not print request/response bodies or exception detail on the auth path.
        raise SystemExit(f"container smoke failed: {type(error).__name__}") from None
