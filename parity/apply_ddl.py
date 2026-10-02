#!/usr/bin/env python3
"""把生成的 DDL 应用到目标 PostgreSQL。

不依赖 psycopg2 —— 用 `docker compose exec psql` 执行，这样本机不需要装
PostgreSQL 客户端驱动，Python 侧也不多一个二进制依赖。

默认在**隔离 schema** 内建表而不是 public：集成测试要反复建表，
隔离 schema 只需 DROP SCHEMA ... CASCADE，不会污染 public。

用法：
    python apply_ddl.py                    # 起库并建表
    python apply_ddl.py --drop             # 先删 schema 再建
    python apply_ddl.py --print            # 只打印要执行的 SQL
    python apply_ddl.py --skip-up          # 不执行 docker compose up
"""

import argparse
import os
import subprocess
import sys
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCHEMA_SQL = os.path.join(REPO, "docker", "schema.sql")
COMPOSE = os.path.join(REPO, "docker-compose.yml")

SERVICE = "postgres"
DB_USER = "sakuramedia"
DB_NAME = "sakuramedia_test"


def run(cmd, **kwargs):
    return subprocess.run(cmd, **kwargs)


def compose(*args, check=True):
    return run(["docker", "compose", "-f", COMPOSE, *args], cwd=REPO, check=check)


def engine_ready():
    r = run(
        ["docker", "version", "--format", "{{.Server.Version}}"],
        capture_output=True,
        text=True,
    )
    return r.returncode == 0 and bool(r.stdout.strip())


def ensure_up():
    """起库并等待服务真正接受连接。

    容器创建成功 != 服务已接受连接，所以要显式轮询 pg_isready ——
    否则后续 psql 会撞上「数据库系统正在启动」。
    """
    if not engine_ready():
        print(
            "Docker engine is not running. Start Docker Desktop first.",
            file=sys.stderr,
        )
        return False
    r = compose("up", "-d", check=False)
    if r.returncode != 0:
        print("docker compose up failed", file=sys.stderr)
        return False

    for _ in range(30):
        probe = run(
            [
                "docker", "compose", "-f", COMPOSE, "exec", "-T", SERVICE,
                "pg_isready", "-U", DB_USER, "-d", DB_NAME,
            ],
            cwd=REPO,
            capture_output=True,
            text=True,
        )
        if probe.returncode == 0:
            print("postgres is accepting connections")
            return True
        time.sleep(1)
    print("postgres did not become ready within 30s", file=sys.stderr)
    return False


def psql(sql, check=True):
    cmd = [
        "docker", "compose", "-f", COMPOSE, "exec", "-T", SERVICE,
        "psql", "-v", "ON_ERROR_STOP=1", "-U", DB_USER, "-d", DB_NAME,
    ]
    return run(cmd, input=sql, text=True, cwd=REPO, check=check)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--schema", default="sakuramedia_test")
    ap.add_argument("--drop", action="store_true", help="先 DROP SCHEMA")
    ap.add_argument("--print", dest="show", action="store_true", help="只打印 SQL")
    ap.add_argument("--skip-up", action="store_true", help="不执行 compose up")
    args = ap.parse_args()

    if not os.path.exists(SCHEMA_SQL):
        print("schema.sql missing; run parity/gen_ddl.py first", file=sys.stderr)
        return 2

    with open(SCHEMA_SQL, "r", encoding="utf-8") as fh:
        ddl = fh.read()

    if args.show:
        print(ddl)
        return 0

    if not args.skip_up and not ensure_up():
        return 1

    # 隔离 schema：先建后用。DDL 里的表名不带 schema 前缀，
    # 靠 search_path 落到目标 schema。
    steps = []
    if args.drop:
        steps.append("DROP SCHEMA IF EXISTS %s CASCADE;" % args.schema)
    steps.append("CREATE SCHEMA IF NOT EXISTS %s;" % args.schema)
    steps.append("SET search_path TO %s;" % args.schema)
    steps.append(ddl)

    r = psql("\n".join(steps), check=False)
    if r.returncode != 0:
        return 1

    print("\napplied DDL into schema %s" % args.schema)
    psql(
        "SELECT count(*) AS tables FROM information_schema.tables "
        "WHERE table_schema = '%s';" % args.schema,
        check=False,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
