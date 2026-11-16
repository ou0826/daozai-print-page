"""只读状态桥测试（不依赖打印机、不需要 Tk）。

运行: python tests/test_bridge.py
"""
from __future__ import annotations

import json
import sys
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

for _s in (sys.stdout, sys.stderr):
    try:
        if _s is not None and hasattr(_s, "reconfigure"):
            _s.reconfigure(errors="replace")
    except Exception:
        pass

from bambu_monitor.bridge import StatusBridge  # noqa: E402
from bambu_monitor.state import Watcher  # noqa: E402


def frame(**kw) -> dict:
    return {"print": kw}


def check(name: str, cond: bool, extra: str = "") -> bool:
    print(("  ok   " if cond else "  FAIL ") + name + (f"  <- {extra}" if extra and not cond else ""))
    return cond


def get(port: int, path: str):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=5) as r:
        return r.status, json.loads(r.read().decode("utf-8"))


def test_status() -> bool:
    print("状态快照")
    w = Watcher()
    w.feed(frame(gcode_state="RUNNING", mc_percent=37, mc_remaining_time=42,
                 layer_num=118, total_layer_num=320, subtask_name="benchy.3mf",
                 nozzle_temper=214.5, bed_temper=59.8))
    bridge = StatusBridge(prefer=41500)
    if not bridge.start():
        return check("状态桥能启动", False)
    try:
        bridge.publish(w.summary(), "Good Mini", "A1 mini")
        code, doc = get(bridge.port, "/status")
        s = doc.get("status") or {}
        ok = check("HTTP 200", code == 200)
        ok &= check("带 api 版本", doc.get("api") == 1, str(doc.get("api")))
        ok &= check("带打印机名与机型",
                    doc.get("printer") == {"name": "Good Mini", "model": "A1 mini"},
                    str(doc.get("printer")))
        ok &= check("进度透传", s.get("percent") == 37, str(s.get("percent")))
        ok &= check("剩余时间透传", s.get("remaining") == 42, str(s.get("remaining")))
        ok &= check("文件名透传", s.get("file") == "benchy.3mf", str(s.get("file")))
        ok &= check("层数透传", s.get("layer") == 118 and s.get("total_layer") == 320)
        ok &= check("温度透传", s.get("nozzle") == 214.5 and s.get("bed") == 59.8)
        ok &= check("age 由服务端算", isinstance(doc.get("age"), (int, float)) and doc["age"] >= 0)
        return ok
    finally:
        bridge.stop()


def test_health_and_404() -> bool:
    print("健康检查与未知路径")
    bridge = StatusBridge(prefer=41510)
    if not bridge.start():
        return check("状态桥能启动", False)
    try:
        code, doc = get(bridge.port, "/health")
        ok = check("/health 返回 ok", code == 200 and doc.get("ok") is True, str(doc))
        try:
            get(bridge.port, "/nope")
            ok &= check("未知路径 404", False)
        except urllib.error.HTTPError as e:
            ok &= check("未知路径 404", e.code == 404, str(e.code))
        return ok
    finally:
        bridge.stop()


def test_empty_is_valid_json() -> bool:
    print("还没连上打印机时也要能读")
    bridge = StatusBridge(prefer=41520)
    if not bridge.start():
        return check("状态桥能启动", False)
    try:
        code, doc = get(bridge.port, "/status")
        return check("空快照仍是合法 JSON", code == 200 and isinstance(doc.get("status"), dict))
    finally:
        bridge.stop()


def test_port_fallback() -> bool:
    print("端口占用时往后挪")
    first = StatusBridge(prefer=41530)
    second = StatusBridge(prefer=41530)
    if not first.start():
        return check("第一个能启动", False)
    try:
        if not second.start():
            return check("端口被占用时自动换端口", False)
        try:
            return check("换了端口", second.port != first.port,
                         f"{first.port} / {second.port}")
        finally:
            second.stop()
    finally:
        first.stop()


def main() -> int:
    ok = True
    for fn in (test_status, test_health_and_404, test_empty_is_valid_json, test_port_fallback):
        ok &= fn()
    print("\n全部通过" if ok else "\n有失败项")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
