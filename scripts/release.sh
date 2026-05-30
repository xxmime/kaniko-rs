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

# 构建macOS版本
echo "构建 macOS 版本..."
cargo build --release
cp target/release/$BINARY_NAME $RELEASE_DIR/${BINARY_NAME}-darwin-amd64-$VERSION
chmod +x $RELEASE_DIR/${BINARY_NAME}-darwin-amd64-$VERSION

# 显示信息
echo ""
echo "发布完成！"
echo "============"
echo "版本: $VERSION"
echo "macOS 版本大小: $(ls -lh $RELEASE_DIR/${BINARY_NAME}-darwin-amd64-$VERSION | awk '{print $5}')"
echo ""
echo "发布文件位置: $RELEASE_DIR/"
echo ""
echo "使用方法:"
echo "  ./${BINARY_NAME}-darwin-amd64-$VERSION --help"
echo ""
echo "注意：如需 Linux 版本，请使用 Docker 构建："
echo "  docker build -f Dockerfile.linux -t kaniko-rust-linux ."
echo "  docker run --rm -v $(pwd)/release:/output kaniko-rust-linux cp /kaniko-cli /output/kaniko-cli-linux-amd64-${VERSION}"