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

echo "── 1/5 fmt ──"
cargo fmt --all --check

echo "── 2/5 clippy（--keep-going：不带它时第一个判红的 crate 会挡住所有下游）──"
cargo clippy --workspace --all-targets --locked --keep-going -- -D warnings

echo "── 3/5 test ──"
cargo test --workspace --locked

echo "── 4/5 暂存区内容自己过得了 fmt ──"
TMPD=$(mktemp -d)
trap 'rm -rf "$TMPD"' EXIT
git checkout-index -a -f --prefix="$TMPD/"
if ! (cd "$TMPD" && cargo fmt --all --check); then
    echo "^^^ 暂存区里的内容过不了 fmt。"
    echo "    修法：cargo fmt --all → git add -A → 再跑本脚本。"
    echo "    （别只格式化工作树就提交：那正是上一轮翻掉的那一步。）"
    exit 1
fi

echo "── 5/5 跨平台编译（抓 cfg 门控漏出来的死代码）──"
# 本机是 windows-gnu，第 2 步看不见"只在非 Windows 上成为 dead_code"这类错——
# `space::existing_ancestor` 就是这么让 CI 的 ubuntu lint 门判红、而本地全绿的。
#
# 本机**编不出全工作区**的 linux 产物：两个 C 依赖的构建脚本要跨端编译器
# （ring←russh←fmnr-remote，zstd-sys←fmnr-archive），这台机器没有 x86_64-linux-gnu-gcc。
# 所以按**实测**分两半（逐个包跑过 `--target $LT` 得到的 PASS/FAIL，不是推断）：
# 吃得下跨端编译的包必须绿；吃不下的点名列出，它们的非 Windows 编译只有 CI 的 ubuntu job 判得动。
LT=x86_64-unknown-linux-gnu
CROSS_OK="fmnr-core fmnr-fs fmnr-pkg fmnr-icons fmnr-plugins fmnr-ai fmnr-bench"
CROSS_SKIPPED="fmnr-ui fmnr-cli fmnr-mount fmnr-remote fmnr-archive"
if rustup target list --installed | grep -q "^$LT\$"; then
    for c in $CROSS_OK; do
        cargo clippy -p "$c" --all-targets --locked --target "$LT" --keep-going -- -D warnings
    done
    echo "跨平台 clippy：跑过 $LT（$CROSS_OK）"
    echo "未覆盖（本机缺跨端 C 编译器，ring 的构建脚本过不去）：$CROSS_SKIPPED"
    echo "      这几个包的非 Windows 编译**只有 CI 的 ubuntu job 判得动**。"
else
    echo "跳过：本机没装 $LT 目标，所以「非 Windows 上才出现的错」这一类**本地判不出来**。"
    echo "      要补上这条能力：rustup target add $LT"
fi

echo "gate: 全绿（第 5 步若显示跳过，则跨平台那一类未覆盖）"
