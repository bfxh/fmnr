#!/bin/sh
# 本地门禁：按 CI 的顺序跑，任何一步判红就停在这里。
#
# 为什么要有这个脚本，而不是"记得跑一遍"：本轮真实踩过一次
# 「本地全绿但 CI 红」——我把 `git add` 跑在 `cargo fmt --all` **之前**，
# 于是格式化改出来的东西留在工作树里没进暂存区，提交上去的是改之前的版本。
# 那种错不是"没跑门"，是"跑门的对象不对"：门判的是工作树，提交的是索引。
#
# 所以除了照抄 CI 的三步，这里多一道 `gate_staged_content_is_formatted`：
# 把**索引**物化成一份干净的树，对它跑 fmt --check。
# 判的是"将要被提交的内容"，不是"我现在屏幕上看到的文件"。
set -eu
cd "$(dirname "$0")/.."

echo "── 1/4 fmt ──"
cargo fmt --all --check

echo "── 2/4 clippy（--keep-going：不带它时第一个判红的 crate 会挡住所有下游）──"
cargo clippy --workspace --all-targets --locked --keep-going -- -D warnings

echo "── 3/4 test ──"
cargo test --workspace --locked

echo "── 4/4 暂存区内容自己过得了 fmt ──"
TMPD=$(mktemp -d)
trap 'rm -rf "$TMPD"' EXIT
git checkout-index -a -f --prefix="$TMPD/"
if ! (cd "$TMPD" && cargo fmt --all --check); then
    echo "^^^ 暂存区里的内容过不了 fmt。"
    echo "    修法：cargo fmt --all → git add -A → 再跑本脚本。"
    echo "    （别只格式化工作树就提交：那正是上一轮翻掉的那一步。）"
    exit 1
fi

echo "gate: 全绿"
