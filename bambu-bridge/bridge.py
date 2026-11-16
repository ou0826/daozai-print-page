"""只读本地状态桥。

把 `Watcher.summary()` 的最新快照以 HTTP + JSON 暴露在 127.0.0.1 上，供其他
程序读取实时打印进度（岛仔 Daozai 的「打印」页就是靠它取数的）。

只监听回环地址、只有 GET、没有写接口，因此不需要鉴权，也不会把凭据或打印
机地址吐出去。

契约（`GET /status`）::

    {
      "api": 1,
      "source": "bambumon",
      "updated": 1763300000.0,   # 最后一次快照的 Unix 时间
      "age": 1.23,               # 距今多少秒；服务端算好，客户端不必对表
      "printer": {"name": "Good Mini", "model": "A1 mini"},
      "status": { ...Watcher.summary() 的字段，原样透传... }
    }

监听端口默认 39778。被占用时自动往后试几个，并把实际端口写到
``%APPDATA%\\BambuMonitor\\rt\\bridge.json``，客户端据此发现。
"""
from __future__ import annotations

import json
import logging
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse

LOG = logging.getLogger(__name__)

DEFAULT_PORT = 39778
PORT_SPAN = 8          # 默认端口往后试这么多个
API_VERSION = 1

_EMPTY = {"api": API_VERSION, "source": "bambumon", "updated": 0.0, "age": 0.0,
          "printer": {"name": "", "model": ""}, "status": {}}


def _rt_dir() -> Path:
    from .config import app_dir
    d = app_dir() / "rt"
    try:
        d.mkdir(parents=True, exist_ok=True)
    except OSError:
        return app_dir()
    return d


class _Server(ThreadingHTTPServer):
    # Windows 的 SO_REUSEADDR 语义和 Unix 不同：它允许直接抢占同一个端口，
    # 于是「端口被占用」不会报错，两个进程会静默抢一个端口。这里必须关掉。
    allow_reuse_address = False
    daemon_threads = True


class _Handler(BaseHTTPRequestHandler):
    server_version = "bambumon-bridge/1"

    def do_GET(self) -> None:  # noqa: N802 (BaseHTTPRequestHandler 命名)
        path = urlparse(self.path).path.rstrip("/") or "/"
        bridge: StatusBridge = self.server.bridge  # type: ignore[attr-defined]
        if path in ("/", "/status"):
            body, code = bridge.body(), 200
        elif path == "/health":
            body, code = b'{"ok":true}', 200
        else:
            body, code = b'{"error":"not found"}', 404
        try:
            self.send_response(code)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, fmt: str, *args) -> None:
        # 客户端每次刷新都会来一趟，访问日志留到 DEBUG 就够了
        LOG.debug("bridge: " + fmt, *args)


class StatusBridge:
    """把最新的打印状态挂在本地端口上。线程安全：publish 只换引用。"""

    def __init__(self, prefer: int = DEFAULT_PORT) -> None:
        self.prefer = int(prefer or DEFAULT_PORT)
        self._lock = threading.Lock()
        self._body = json.dumps(_EMPTY).encode("utf-8")
        self._updated = 0.0
        self._server: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None
        self.port = 0

    # ---------- 发布 ----------
    def publish(self, summary: dict, name: str = "", model: str = "", online: bool | None = None) -> None:
        """由 Tk 线程调用，更新快照。序列化失败时保留上一份，不影响主流程。

        `online` 用界面自己的连接状态覆盖：`Watcher.summary()` 里的 `online`
        字段从未被赋值（一直是 False），界面判断连没连上用的是 App._connected。
        """
        status = dict(summary) if isinstance(summary, dict) else {}
        if online is not None:
            status["online"] = bool(online)
        doc = {
            "api": API_VERSION,
            "source": "bambumon",
            "updated": time.time(),
            "printer": {"name": str(name or ""), "model": str(model or "")},
            "status": status,
        }
        try:
            body = json.dumps(doc, ensure_ascii=False).encode("utf-8")
        except (TypeError, ValueError):
            LOG.debug("状态快照无法序列化，保留上一份", exc_info=True)
            return
        with self._lock:
            self._body = body
            self._updated = doc["updated"]

    def body(self) -> bytes:
        """当前快照，并补上服务端算好的 age（客户端不用处理时钟偏差）。"""
        with self._lock:
            raw, updated = self._body, self._updated
        age = round(time.time() - updated, 2) if updated else 0.0
        try:
            doc = json.loads(raw)
        except ValueError:
            return raw
        doc["age"] = age
        try:
            return json.dumps(doc, ensure_ascii=False).encode("utf-8")
        except (TypeError, ValueError):
            return raw

    # ---------- 生命周期 ----------
    def start(self) -> bool:
        """起服务；端口占用时往后试。成功则写 rt/bridge.json 供客户端发现。"""
        for port in range(self.prefer, self.prefer + PORT_SPAN):
            try:
                server = _Server(("127.0.0.1", port), _Handler)
            except OSError:
                continue
            server.bridge = self  # type: ignore[attr-defined]
            self._server = server
            self.port = port
            self._thread = threading.Thread(
                target=server.serve_forever, name="bambumon-bridge", daemon=True
            )
            self._thread.start()
            self._announce()
            LOG.info("状态桥已启动: http://127.0.0.1:%s/status", port)
            return True
        LOG.warning("状态桥启动失败：%s~%s 端口都被占用", self.prefer,
                    self.prefer + PORT_SPAN - 1)
        return False

    def stop(self) -> None:
        server, self._server = self._server, None
        if server is not None:
            try:
                server.shutdown()
                server.server_close()
            except Exception:
                LOG.debug("关闭状态桥出错", exc_info=True)
        self.port = 0
        try:
            (_rt_dir() / "bridge.json").unlink(missing_ok=True)
        except OSError:
            pass

    def _announce(self) -> None:
        doc = {"port": self.port, "api": API_VERSION, "pid": _pid()}
        try:
            (_rt_dir() / "bridge.json").write_text(
                json.dumps(doc, ensure_ascii=False), encoding="utf-8"
            )
        except OSError:
            LOG.debug("写 bridge.json 失败，客户端可退回默认端口", exc_info=True)


def _pid() -> int:
    import os
    return os.getpid()


def discover() -> int:
    """读 rt/bridge.json 里的端口；读不到就返回默认端口。"""
    try:
        raw = (_rt_dir() / "bridge.json").read_text(encoding="utf-8")
        port = int(json.loads(raw).get("port") or 0)
        if 0 < port < 65536:
            return port
    except (OSError, ValueError, TypeError):
        pass
    return DEFAULT_PORT
