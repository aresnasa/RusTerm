#!/usr/bin/env python3
"""并发访问 /v1/models 的命令行工具。

零第三方依赖（仅标准库）。用法示例：

    python3 concurrent_request.py                     # 默认 10 并发 / 50 请求
    python3 concurrent_request.py -c 50 -n 200        # 50 并发, 共 200 请求
    python3 concurrent_request.py -c 20 -n 100 -v     # 打印每次请求耗时

如需更高并发/更高吞吐，推荐改用 asyncio + aiohttp（见文件末尾注释）。
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed

DEFAULT_URL = "http://token-api.zs.shaipower.online/v1/models"
DEFAULT_TOKEN = "abc"


def do_request(url: str, token: str, timeout: float) -> tuple[bool, float, str]:
    """发起一次请求。返回 (是否成功, 耗时秒, 说明)。"""
    req = urllib.request.Request(
        url,
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/json",
        },
        method="GET",
    )
    start = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            # 读取并丢弃正文（顺便确认连接正常）
            resp.read()
        elapsed = time.perf_counter() - start
        return True, elapsed, str(resp.status)
    except urllib.error.HTTPError as e:
        elapsed = time.perf_counter() - start
        return False, elapsed, f"HTTP {e.code}"
    except Exception as e:  # noqa: BLE001
        elapsed = time.perf_counter() - start
        return False, elapsed, f"{type(e).__name__}: {e}"


def main() -> int:
    ap = argparse.ArgumentParser(description="并发访问 /v1/models")
    ap.add_argument("-u", "--url", default=DEFAULT_URL, help="目标 URL")
    ap.add_argument("-t", "--token", default=DEFAULT_TOKEN, help="Bearer token")
    ap.add_argument(
        "-c", "--concurrency", type=int, default=10, help="并发数 (默认 10)"
    )
    ap.add_argument(
        "-n", "--total", type=int, default=50, help="请求总数 (默认 50)"
    )
    ap.add_argument("--timeout", type=float, default=30.0, help="单请求超时秒数")
    ap.add_argument(
        "-v", "--verbose", action="store_true", help="打印每次请求结果"
    )
    args = ap.parse_args()

    if args.concurrency <= 0 or args.total <= 0:
        print("concurrency 和 total 必须为正数", file=sys.stderr)
        return 2

    print(
        f"目标: {args.url}\n"
        f"并发: {args.concurrency}  总数: {args.total}  超时: {args.timeout}s",
        file=sys.stderr,
    )

    latencies: list[float] = []
    ok = 0
    fail = 0
    errors: dict[str, int] = {}

    wall_start = time.perf_counter()
    with ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        futures = [
            pool.submit(do_request, args.url, args.token, args.timeout)
            for _ in range(args.total)
        ]
        for i, fut in enumerate(as_completed(futures), 1):
            success, elapsed, note = fut.result()
            latencies.append(elapsed)
            if success:
                ok += 1
            else:
                fail += 1
                errors[note] = errors.get(note, 0) + 1
            if args.verbose:
                tag = "OK " if success else "ERR"
                print(f"[{i:>4}/{args.total}] {tag} {elapsed*1000:8.1f}ms  {note}")
    wall = time.perf_counter() - wall_start

    # 汇总统计
    latencies.sort()
    print("\n==== 结果 ====")
    print(f"成功 / 失败 : {ok} / {fail}")
    print(f"总耗时      : {wall:.2f}s")
    print(f"吞吐        : {args.total / wall:.1f} req/s")
    if latencies:
        avg = statistics.mean(latencies)
        p = lambda q: latencies[min(len(latencies) - 1, int(q * len(latencies)))]
        print(
            f"延迟 (ms)   : "
            f"min={latencies[0]*1000:.1f}  "
            f"avg={avg*1000:.1f}  "
            f"p50={p(0.50)*1000:.1f}  "
            f"p90={p(0.90)*1000:.1f}  "
            f"p99={p(0.99)*1000:.1f}  "
            f"max={latencies[-1]*1000:.1f}"
        )
    if errors:
        print("错误分布    :")
        for note, cnt in sorted(errors.items(), key=lambda x: -x[1]):
            print(f"  {cnt:>5}  {note}")

    return 0 if fail == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
