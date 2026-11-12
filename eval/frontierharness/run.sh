#!/usr/bin/env bash
# 一条命令跑 moc 的 FrontierHarness 评测：
#
#   eval/frontierharness/run.sh <base_url> <模型id> <api_key> [api格式] [harbor 额外参数...]
#
# 默认跑 Runta 云环境（官方 v1.1 推荐路径）；切本地环境用
# `HARBOR_ENV=<名字> run.sh ...` 或末尾透传 `-e <名字>`（同名 flag 后者覆盖前者）。
#
# 例：
#   eval/frontierharness/run.sh \
#     https://api.fireworks.ai/inference/v1 \
#     accounts/fireworks/models/kimi-k3 \
#     sk-xxxx openai_chat \
#     -i 'regex-log' --jobs-dir ./jobs
#
# api 格式可选 openai_chat（默认）/ openai_responses / anthropic_messages；
# 之后的参数原样透传给 harbor run（如 -i 任务名glob、-l 任务数上限、
# -n 并发数、-x 排除、--agent-timeout-multiplier 超时倍率等）。
set -euo pipefail

usage() {
    echo "用法: $0 <base_url> <模型id> <api_key> [api格式] [harbor 参数...]" >&2
    exit 2
}

[ $# -ge 3 ] || usage
BASE_URL=$1
MODEL=$2
API_KEY=$3
shift 3

API_FORMAT=openai_chat
if [ $# -gt 0 ] && [[ $1 != -* ]]; then
    API_FORMAT=$1
    shift
fi

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$HERE/../.."   # 仓库根：binary 默认 target/release/moc、jobs-dir 便于定位

exec env PYTHONPATH="$HERE" harbor run \
    -d frontier-harness-eval/frontier-harness-eval@v1.1.0 \
    -a moc_agent:MocAgent \
    --ak "moc_base_url=$BASE_URL" \
    --ak "model=$MODEL" \
    --ak "api_key=$API_KEY" \
    --ak "api_format=$API_FORMAT" \
    -e "${HARBOR_ENV:-runta}" --override-storage-mb 32768 \
    "$@"
