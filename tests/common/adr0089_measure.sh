#!/usr/bin/env bash
# ADR-0089 实测驱动脚本（命令即记录）。用法见 docs/research/protocol-http-benchmark.md 的 ADR-0089 一节。
#
#   adr0089_measure.sh counts  BINARY LABEL   # 计数队列：与机器负载无关，可随时运行
#   adr0089_measure.sh lease   BINARY LABEL   # SQLite 写租约排队：只在机器空闲后运行
#   adr0089_measure.sh pg      BINARY LABEL   # PostgreSQL fixed-workers 32 尾延迟：只在机器空闲后运行
#   adr0089_measure.sh sweep   BINARY LABEL   # history.sweep 耗时：稳态 + 调低 N 后首次（DATA_DIR 复用已升级库，否则现场灌小队列）
#   adr0089_measure.sh contention BINARY LABEL # PG 并发写 + 强制 sweep：尾延迟与 FK/序列化错误计数（空闲机运行）
#   adr0089_measure.sh toolflow BINARY LABEL  # 工具调用流（6 轮 × 4 变体）计数；BACKEND=postgres 走 PG
#   adr0089_measure.sh upgrade-gen HARNESS LABEL  # 生成放大的升级前库（BINARY 位置传 storage-test-harness）
#   adr0089_measure.sh verify-fixture BASELINE_BINARY LABEL  # 放大库可被旧服务端经管理 API 读出（SQLite）
#   adr0089_measure.sh upgrade  BINARY LABEL  # 在副本上测新二进制的升级（墙钟/CPU/峰值 RSS/峰值磁盘）
#
# upgrade-gen/upgrade 用 BACKEND=sqlite|postgres（默认 sqlite）。规模默认 events 490000 /
# turn_contents 178000 / markers 17000（ADR-0089 §升级的开发库规模），可用 TARGET_EVENTS、
# TARGET_CONTENTS、TARGET_MARKERS 覆盖；实际达成规模写入 $out/upgrade-source-<backend>.json。
#
# 环境变量：MEASURE_OUT（默认 target/benchmarks/adr0089）、PG_ADMIN_URL（默认
# postgresql://postgres@127.0.0.1:55432）、REDIS_URL（默认 redis://127.0.0.1:56379/0，PostgreSQL
# 后端的 baseline 与新二进制都要求 Redis，否则 setup 返回 cache_unavailable；必须是专用空实例，
# 每次 PG 运行前由运行者 FLUSHDB；实例与其他任务共用时只清 DB 0：redis-cli -p 56379 -n 0 flushdb）。
set -euo pipefail

mode=${1:?mode: counts|lease|pg|sweep|contention|toolflow|upgrade-gen|verify-fixture|upgrade}
binary=${2:?server binary}
label=${3:?label}
out=${MEASURE_OUT:-target/benchmarks/adr0089}
pg_admin=${PG_ADMIN_URL:-postgresql://postgres@127.0.0.1:55432}
redis=${REDIS_URL:-redis://127.0.0.1:56379/0}
mkdir -p "$out"
harness=(uv run --locked --group test python -m tests.common.measure_protocol_http)
sha=$(sha256sum "$binary" | cut -d' ' -f1)
echo "binary=$binary sha256=$sha label=$label" | tee "$out/$label-$mode.binary"

# 一个 Debug epoch 的 perf 环形缓冲只有 10000 个 completed span / 2048 个 SQL span，
# 因此每个 case 的 Run 数保持在 4（计数）或 8（租约）以内；任何 dropped_* 非零即作废。
case_flags=(--mode fixed-workers --warmup 0 --min-first-token-delay 1 --max-first-token-delay 1
            --min-duration 10 --max-duration 10 --min-tokens-per-second 30 --max-tokens-per-second 30 --seed 1)

case "$mode" in
counts)
  rm -rf "$out/$label-counts-data" "$out/$label-counts-perf"
  "${harness[@]}" --binary "$binary" --label "$label" --output "$out/$label-counts.json" \
    --ingress openai-chat anthropic-messages --upstream openai-chat anthropic-messages \
    --delivery unary stream --concurrency 1 --requests-per-case 4 "${case_flags[@]}" \
    --data-dir "$out/$label-counts-data" --save-performance-snapshots "$out/$label-counts-perf"
  uv run --locked --group test python -m tests.common.analyze_observation_writes \
    --result "$out/$label-counts.json" --snapshots "$out/$label-counts-perf" \
    --data-dir "$out/$label-counts-data" --output "$out/$label-counts-analysis.json"
  ;;
lease)
  rm -rf "$out/$label-lease-data" "$out/$label-lease-perf"
  "${harness[@]}" --binary "$binary" --label "$label" --output "$out/$label-lease.json" \
    --ingress openai-chat --upstream openai-chat --delivery unary stream \
    --concurrency 8 --requests-per-case 16 "${case_flags[@]}" \
    --data-dir "$out/$label-lease-data" --save-performance-snapshots "$out/$label-lease-perf"
  uv run --locked --group test python -m tests.common.analyze_observation_writes \
    --result "$out/$label-lease.json" --snapshots "$out/$label-lease-perf" \
    --data-dir "$out/$label-lease-data" --output "$out/$label-lease-analysis.json"
  ;;
pg)
  db="stravia_benchmark_${label//[^a-zA-Z0-9]/_}"
  psql "$pg_admin/postgres" -v ON_ERROR_STOP=1 -c "DROP DATABASE IF EXISTS $db" -c "CREATE DATABASE $db"
  extra=(--redis-url "$redis")
  "${harness[@]}" --binary "$binary" --label "$label" --output "$out/$label-pg.json" \
    --ingress openai-chat --upstream openai-chat --delivery unary stream --mode fixed-workers \
    --concurrency 32 --warmup 5 --duration 120 --seed 1 \
    --backend postgres --database-url "$pg_admin/$db" --confirm-isolated-database "${extra[@]}" || status=$?
  psql "$pg_admin/postgres" -v ON_ERROR_STOP=1 -c "DROP DATABASE $db"
  exit "${status:-0}"
  ;;
sweep)
  # 基线二进制没有 history.sweep：SWEEP_SPAN=observation.maintenance.sweep
  sweep_flags=(--steady-repeats "${STEADY_REPEATS:-3}" --lower-to "${LOWER_TO:-1}")
  [[ -n ${SWEEP_SPAN:-} ]] && sweep_flags+=(--sweep-span "$SWEEP_SPAN")
  if [[ ${BACKEND:-sqlite} == postgres ]]; then
    db=${PG_DATABASE:?PG_DATABASE: 已升级的（或现场新建的）PG 库名}
    uv run --locked --group test python -m tests.common.measure_history_sweep --binary "$binary" --label "$label" \
      --output "$out/$label-sweep-pg.json" --backend postgres --database-url "$pg_admin/$db" \
      --confirm-isolated-database --redis-url "$redis" "${sweep_flags[@]}" ${DATA_DIR:+--reuse-data-dir --data-dir "$DATA_DIR"}
  elif [[ -n ${DATA_DIR:-} ]]; then
    uv run --locked --group test python -m tests.common.measure_history_sweep --binary "$binary" --label "$label" \
      --output "$out/$label-sweep-sqlite.json" --reuse-data-dir --data-dir "$DATA_DIR" --startup-timeout 3600 "${sweep_flags[@]}"
  else
    rm -rf "$out/$label-sweep-data"
    uv run --locked --group test python -m tests.common.measure_history_sweep --binary "$binary" --label "$label" \
      --output "$out/$label-sweep-sqlite.json" --data-dir "$out/$label-sweep-data" --cohort-requests "${COHORT_REQUESTS:-8}" "${sweep_flags[@]}"
  fi
  ;;
contention)
  db="stravia_contention_${label//[^a-zA-Z0-9]/_}"
  psql "$pg_admin/postgres" -v ON_ERROR_STOP=1 -c "DROP DATABASE IF EXISTS $db" -c "CREATE DATABASE $db"
  sweep_flags=()
  [[ -n ${SWEEP_SPAN:-} ]] && sweep_flags+=(--sweep-span "$SWEEP_SPAN")
  uv run --locked --group test python -m tests.common.measure_pg_contention --binary "$binary" --label "$label" \
    --output "$out/$label-contention-pg.json" --backend postgres --database-url "$pg_admin/$db" \
    --confirm-isolated-database --redis-url "$redis" --workers "${WORKERS:-32}" --duration "${DURATION:-120}" \
    "${sweep_flags[@]}" || status=$?
  psql "$pg_admin/postgres" -v ON_ERROR_STOP=1 -c "DROP DATABASE $db"
  exit "${status:-0}"
  ;;
toolflow)
  backend=${BACKEND:-sqlite}
  if [[ $backend == postgres ]]; then
    db="stravia_toolflow_${label//[^a-zA-Z0-9]/_}"
    psql "$pg_admin/postgres" -v ON_ERROR_STOP=1 -c "DROP DATABASE IF EXISTS $db" -c "CREATE DATABASE $db"
    extra=(--redis-url "$redis")
    uv run --locked --group test python -m tests.common.measure_tool_flow --binary "$binary" --label "$label" --output "$out/$label-toolflow-pg.json" \
      --conversations "${CONVERSATIONS:-8}" --backend postgres --database-url "$pg_admin/$db" \
      --confirm-isolated-database "${extra[@]}" || status=$?
    psql "$pg_admin/postgres" -v ON_ERROR_STOP=1 -c "DROP DATABASE $db"
    exit "${status:-0}"
  fi
  rm -rf "$out/$label-toolflow-data" "$out/$label-toolflow.json.snapshots"
  uv run --locked --group test python -m tests.common.measure_tool_flow --binary "$binary" --label "$label" \
    --output "$out/$label-toolflow.json" --conversations "${CONVERSATIONS:-8}" --data-dir "$out/$label-toolflow-data"
  ;;
upgrade-gen)
  backend=${BACKEND:-sqlite}
  scale=(--target-events "${TARGET_EVENTS:-490000}" --target-contents "${TARGET_CONTENTS:-178000}"
         --target-markers "${TARGET_MARKERS:-17000}")
  if [[ $backend == postgres ]]; then
    db="stravia_upgrade_source"
    psql "$pg_admin/postgres" -v ON_ERROR_STOP=1 -c "DROP DATABASE IF EXISTS $db" -c "CREATE DATABASE $db"
    uv run --locked --group test python -m tests.common.scale_upgrade_fixture --backend postgres \
      --database-url "$pg_admin/$db" --storage-harness "$binary" "${scale[@]}" --output "$out/upgrade-source-postgres.json"
  else
    rm -rf "$out/upgrade-source-sqlite"
    uv run --locked --group test python -m tests.common.scale_upgrade_fixture --backend sqlite \
      --data-dir "$out/upgrade-source-sqlite" --storage-harness "$binary" "${scale[@]}" --output "$out/upgrade-source-sqlite.json"
  fi
  ;;
verify-fixture)
  # 用 BINARY（实施前二进制）在副本上经管理 API 读出抽样交互，证明放大库是旧服务端可读的真实形态。SQLite 专用。
  rm -rf "$out/verify-work-$label"
  uv run --locked --group test python -m tests.common.verify_scaled_fixture --binary "$binary" \
    --source "$out/upgrade-source-sqlite" --work "$out/verify-work-$label"
  rm -rf "$out/verify-work-$label"
  ;;
upgrade)
  backend=${BACKEND:-sqlite}
  keep=()
  [[ -n ${KEEP_UPGRADED:-} ]] && keep=(--keep)   # 升级后的副本留给 sweep 模式复用（SQLite 目录 / PG 库 <源库>_<label>）
  if [[ $backend == postgres ]]; then
    uv run --locked --group test python -m tests.common.measure_upgrade --binary "$binary" --backend postgres \
      --source stravia_upgrade_source --pg-admin-url "$pg_admin/postgres" --redis-url "$redis" \
      --label "$label" --output "$out/$label-upgrade-postgres.json" "${keep[@]}"
  else
    rm -rf "$out/upgrade-work-$label"
    uv run --locked --group test python -m tests.common.measure_upgrade --binary "$binary" --backend sqlite \
      --source "$out/upgrade-source-sqlite" --work "$out/upgrade-work-$label" \
      --label "$label" --output "$out/$label-upgrade-sqlite.json" "${keep[@]}"
  fi
  ;;
*) echo "unknown mode $mode" >&2; exit 2 ;;
esac
