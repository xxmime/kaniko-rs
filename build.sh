#!/bin/bash

set -e

# 默认构建模式
MODE="${1:-release}"

usage() {
    echo "用法: $0 [release|static|docker-static|help]"
    echo ""
    echo "  release       - 发布构建 (动态链接, 默认)"
    echo "  static        - 静态链接构建 (需要 musl 目标)"
    echo "  docker-static - Docker 内静态链接构建"
    echo "  help          - 显示此帮助"
}

case "$MODE" in
    release)
        echo "=== 发布构建 (动态链接) ==="
        git pull
        make build-release
        rm -f /usr/local/bin/kaniko-cli
        mv target/release/kaniko-cli /usr/local/bin

        echo "build test..."
        kaniko-cli --force --sandbox --dockerfile Dockerfile --no-push --destination test.tar

        RUST_LOG_LEVEL=debug kaniko-cli --force --sandbox --dockerfile Dockerfile --destination registry-intl.cn-shanghai.aliyuncs.com/mirror_library/alpine:test
        ;;

    static)
        echo "=== 静态链接构建 (musl) ==="
        # 确保 musl 目标已安装
        rustup target add x86_64-unknown-linux-musl 2>/dev/null || true
        make build-static
        echo ""
        echo "静态链接二进制:"
        ls -lh target/x86_64-unknown-linux-musl/release/kaniko-cli
        file target/x86_64-unknown-linux-musl/release/kaniko-cli
        ;;

    docker-static)
        echo "=== Docker 静态链接构建 ==="
        docker build \
            --build-arg TARGET=x86_64-unknown-linux-musl \
            -f Dockerfile.static \
            -t kaniko-static:latest .
        echo ""
        echo "从容器中提取二进制:"
        echo "  docker create --name kaniko-extract kaniko-static:latest"
        echo "  docker cp kaniko-extract:/kaniko-cli ./kaniko-cli-linux-amd64-static"
        echo "  docker rm kaniko-extract"
        ;;

    help|--help|-h)
        usage
        ;;

    *)
        echo "未知模式: $MODE"
        usage
        exit 1
        ;;
esac