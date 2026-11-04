"""ADR-0089 升级成本实测：新二进制在放大的升级前数据库上完成迁移（setup/complete 内执行 0037+）。

`scale_upgrade_fixture.py` 生成的源库保持不动；每次运行先复制一份（SQLite：复制 data-dir；
PostgreSQL：`CREATE DATABASE … TEMPLATE 源库`），在副本上计时。复制不计入升级耗时。

记录：升级请求墙钟时间、服务端进程 CPU（utime+stime 差值）与峰值 RSS（`/proc/<pid>/status`
的 VmHWM 与 20Hz 采样）、`resource.getrusage(RUSAGE_CHILDREN)`（等价于 `time -v` 的
Maximum resident set size / user / system，本机没有 `/usr/bin/time`，故在专用子进程内取）、
峰值磁盘占用：SQLite 为 data-dir 递归字节数（主库+WAL+SHM），PostgreSQL 为
`pg_database_size` 与逐表 `pg_total_relation_size` 之和及 `pg_ls_waldir()` 字节数。前后各取
一次存储清单（`protocol_storage_inventory`）。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import resource
import shutil
import sys
import threading
import time
from argparse import Namespace
from pathlib import Path
from urllib.parse import urlsplit, urlunsplit

from tests.common.helpers import (
    WebSession, find_free_port, start_stravia_server, stop_stravia_server,
    wait_for_setup_token, wait_until_ready,
)
from tests.common.protocol_storage_inventory import read_storage_inventory


def tree_bytes(path: Path) -> int:
    return sum(f.stat().st_size for f in path.rglob("*") if f.is_file())


def proc_status_kb(pid: int, field: str) -> int:
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith(field + ":"):
            return int(line.split()[1])
    raise RuntimeError(f"{field} missing from /proc/{pid}/status")


def cpu_seconds(pid: int) -> float:
    stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return (int(stat[11]) + int(stat[12])) / os.sysconf("SC_CLK_TCK")


def child_pid(pid: int) -> int:
    return pid


def with_database(url: str, name: str) -> str:
    parts = urlsplit(url)
    return urlunsplit(parts._replace(path="/" + name))


class Sampler(threading.Thread):
    """高频采样：服务端 RSS 与（后端相关的）磁盘占用，保留峰值。"""

    def __init__(self, pid: int, disk, interval: float) -> None:
        super().__init__(daemon=True)
        self.pid, self.disk, self.interval = pid, disk, interval
        self.stop_event = threading.Event()
        self.peak_rss_kb = 0
        self.peak_disk = 0
        self.samples = 0
        self.error: str | None = None

    def run(self) -> None:
        while not self.stop_event.is_set():
            try:
                self.peak_rss_kb = max(self.peak_rss_kb, proc_status_kb(self.pid, "VmRSS"))
                self.peak_disk = max(self.peak_disk, self.disk())
                self.samples += 1
            except Exception as error:  # 采样失败必须出现在结果里，不能静默
                self.error = f"{type(error).__name__}: {error}"
                return
            self.stop_event.wait(self.interval)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, required=True, help="升级后的（新）server 二进制")
    parser.add_argument("--backend", choices=("sqlite", "postgres"), required=True)
    parser.add_argument("--source", required=True,
                        help="SQLite：scale_upgrade_fixture 生成的 data-dir；PostgreSQL：源库名（同一实例）")
    parser.add_argument("--pg-admin-url", default="postgresql://postgres@127.0.0.1:55432/postgres")
    parser.add_argument("--redis-url", help="PostgreSQL 后端的新二进制必需（专用空 Redis）")
    parser.add_argument("--work", help="副本名（SQLite 目录路径 / PG 库名）；默认按标签生成，必须不存在")
    parser.add_argument("--label", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--sample-interval", type=float, default=0.05)
    parser.add_argument("--upgrade-timeout", type=float, default=3600)
    parser.add_argument("--keep", action="store_true", help="保留副本以便检查")
    args = parser.parse_args()

    result: dict = {"command": sys.argv, "label": args.label, "backend": args.backend,
                    "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(), "source": args.source}
    pg = None
    work_dir: Path
    if args.backend == "sqlite":
        source = Path(args.source)
        work_dir = Path(args.work or f"{source}-{args.label}")
        if work_dir.exists():
            parser.error(f"work dir {work_dir} already exists")
        t = time.monotonic()
        shutil.copytree(source, work_dir)
        result["copy_seconds"] = round(time.monotonic() - t, 2)
        database = {"backend": "sqlite"}
        inventory_args = Namespace(backend="sqlite", database_url=None)
        disk = lambda: tree_bytes(work_dir)  # noqa: E731
        directory = work_dir
        server_extra: list[str] = []
        cleanup = lambda: shutil.rmtree(work_dir, ignore_errors=False)  # noqa: E731
    else:
        import psycopg
        work_name = args.work or f"{args.source}_{args.label}".replace("-", "_")
        if not args.redis_url:
            parser.error("--redis-url is required for postgres")
        admin = psycopg.connect(args.pg_admin_url, autocommit=True)
        t = time.monotonic()
        admin.execute(f'CREATE DATABASE "{work_name}" TEMPLATE "{args.source}"')
        result["copy_seconds"] = round(time.monotonic() - t, 2)
        url = with_database(args.pg_admin_url, work_name)
        database = {"backend": "postgres", "url": url}
        inventory_args = Namespace(backend="postgres", database_url=url)
        pg = psycopg.connect(url, autocommit=True)

        def disk() -> int:
            relations = pg.execute(
                "SELECT COALESCE(SUM(pg_total_relation_size(c.oid)), 0) FROM pg_class c "
                "JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind = 'r' AND n.nspname = current_schema()"
            ).fetchone()[0]
            wal = pg.execute("SELECT COALESCE(SUM(size), 0) FROM pg_ls_waldir()").fetchone()[0]
            database_bytes = pg.execute("SELECT pg_database_size(current_database())").fetchone()[0]
            # 组件峰值单独记录：WAL 目录含预分配/回收段，不能与库体积混为一谈。
            peaks = result.setdefault("pg_disk_peak_bytes", {"relations": 0, "wal_dir": 0, "database": 0})
            for name, size in (("relations", relations), ("wal_dir", wal), ("database", database_bytes)):
                peaks[name] = max(peaks[name], int(size))
            return int(database_bytes) + int(wal)

        work_dir = Path(args.output).with_suffix(".data")
        work_dir.mkdir(parents=True, exist_ok=False)
        directory = work_dir
        config = work_dir / "upgrade.toml"
        config.write_text("[cache]\nredis_url = " + json.dumps(args.redis_url) + "\n", encoding="utf-8")
        server_extra = ["--config", str(config)]

        def cleanup() -> None:
            pg.close()
            admin.execute(f'DROP DATABASE "{work_name}"')
            admin.close()
            shutil.rmtree(work_dir)

    result["before"] = read_storage_inventory(inventory_args, directory)
    result["disk_before_bytes"] = disk()

    port = find_free_port()
    base = f"http://127.0.0.1:{port}"
    process, logs = start_stravia_server(
        stravia_binary=args.binary,
        args=["--data-dir", str(work_dir), "--host", "127.0.0.1", "--port", str(port), *server_extra],
        env={"RUST_LOG": "warn,stravia_server=info,stravia_core=info"},
    )
    sampler = None
    try:
        wait_until_ready(f"{base}/api/v1/auth/state")
        token = wait_for_setup_token(logs, process)
        session = WebSession(base)
        status, body = session.request("POST", "/api/v1/setup/claim", {"token": token})
        assert status == 204, (status, body)
        result["idle_rss_kb"] = proc_status_kb(process.pid, "VmRSS")
        cpu_before = cpu_seconds(process.pid)
        sampler = Sampler(process.pid, disk, args.sample_interval)
        sampler.start()
        started = time.perf_counter()
        status, body = session.request(
            "POST", "/api/v1/setup/complete",
            {"database": database, "username": "admin", "password": "correct horse battery staple", "client_base_url": base},
            timeout=args.upgrade_timeout)
        result["upgrade_wall_seconds"] = round(time.perf_counter() - started, 3)
        result["upgrade_http_status"] = status
        sampler.stop_event.set()
        sampler.join()
        result["upgrade_cpu_seconds"] = round(cpu_seconds(process.pid) - cpu_before, 3)
        result["peak_rss_kb_sampled"] = sampler.peak_rss_kb
        result["peak_rss_kb_vmhwm"] = proc_status_kb(process.pid, "VmHWM")
        result["peak_disk_bytes_sampled"] = sampler.peak_disk
        result["samples"] = sampler.samples
        result["sampler_error"] = sampler.error
        if status != 200:
            result["server_log_tail"] = logs[-60:]
            raise RuntimeError(f"setup/complete failed: {status} {body}")
        status, _ = session.request("POST", "/api/v1/auth/login", {"username": "admin", "password": "correct horse battery staple"})
        assert status == 200, status
        result["disk_after_bytes"] = disk()
        result["after"] = read_storage_inventory(inventory_args, directory)
        result["server_log_upgrade_lines"] = [line for line in logs if any(
            word in line.lower() for word in ("migrat", "upgrade", "content"))][-40:]
        result["valid"] = result["sampler_error"] is None and status == 200
    finally:
        if sampler is not None:
            sampler.stop_event.set()
        stop_stravia_server(process, logs, print_tail=0)
        usage = resource.getrusage(resource.RUSAGE_CHILDREN)
        result["rusage_children"] = {"maxrss_kb": usage.ru_maxrss, "utime_seconds": usage.ru_utime,
                                     "stime_seconds": usage.ru_stime,
                                     "note": "等价 `time -v`：整个服务端生命周期，含空闲与关闭"}
        args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
        if not args.keep:
            cleanup()
    print(json.dumps({key: result.get(key) for key in (
        "upgrade_wall_seconds", "upgrade_cpu_seconds", "peak_rss_kb_vmhwm", "peak_disk_bytes_sampled",
        "disk_before_bytes", "disk_after_bytes", "valid")}, indent=2))
    sys.exit(0 if result.get("valid") else 1)


if __name__ == "__main__":
    main()
