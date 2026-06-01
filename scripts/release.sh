#!/bin/bash

# Kaniko Rust 发布脚本

set -e

VERSION=$(grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
BINARY_NAME="kaniko-cli"
RELEASE_DIR="release"

echo "=== Kaniko Rust 发布脚本 ==="
echo "版本: $VERSION"
echo "二进制: $BINARY_NAME"
echo ""

# 创建发布目录
mkdir -p $RELEASE_DIR

# 构建 macOS 版本 (动态链接)
echo "构建 macOS 版本..."
cargo build --release
cp target/release/$BINARY_NAME $RELEASE_DIR/${BINARY_NAME}-darwin-amd64-$VERSION
chmod +x $RELEASE_DIR/${BINARY_NAME}-darwin-amd64-$VERSION

# 构建 Linux 静态链接版本 (x86_64 musl)
echo "构建 Linux x86_64 静态链接版本..."
rustup target add x86_64-unknown-linux-musl 2>/dev/null || true
cargo build --release --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/$BINARY_NAME $RELEASE_DIR/${BINARY_NAME}-linux-amd64-static-$VERSION
chmod +x $RELEASE_DIR/${BINARY_NAME}-linux-amd64-static-$VERSION

# 显示信息
echo ""
echo "发布完成！"
echo "============"
echo "版本: $VERSION"
echo ""
echo "文件列表:"
echo "  macOS:    $RELEASE_DIR/${BINARY_NAME}-darwin-amd64-$VERSION"
echo "            大小: $(ls -lh $RELEASE_DIR/${BINARY_NAME}-darwin-amd64-$VERSION | awk '{print $5}')"
echo "  Linux:    $RELEASE_DIR/${BINARY_NAME}-linux-amd64-static-$VERSION"
echo "            大小: $(ls -lh $RELEASE_DIR/${BINARY_NAME}-linux-amd64-static-$VERSION | awk '{print $5}')"
echo ""
echo "使用方法:"
echo "  ./${BINARY_NAME}-darwin-amd64-$VERSION --help"
echo "  ./${BINARY_NAME}-linux-amd64-static-$VERSION --help"
echo ""
echo "注意："
echo "  - Linux 版本为静态链接，无需额外依赖即可运行"
echo "  - 如需 aarch64 版本，请使用 Docker 构建："
echo "    docker build --build-arg TARGET=aarch64-unknown-linux-musl -f Dockerfile.static -t kaniko-static-arm64 ."