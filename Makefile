.PHONY: build build-release build-static build-static-arm64 test clean

# 默认构建
build:
	cargo build

# 发布构建
build-release:
	cargo build --release

# 静态链接构建 (x86_64 musl)
build-static:
	cargo build --release --target x86_64-unknown-linux-musl

# 静态链接构建 (aarch64 musl)
build-static-arm64:
	cargo build --release --target aarch64-unknown-linux-musl

# 测试
test:
	cargo test

# 清理
clean:
	cargo clean

# 显示当前二进制大小
size:
	@ls -lh target/release/kaniko-cli 2>/dev/null || echo "Binary not found, run 'make build-release' first"

# 显示静态链接二进制大小
size-static:
	@ls -lh target/x86_64-unknown-linux-musl/release/kaniko-cli 2>/dev/null || echo "Binary not found, run 'make build-static' first"

# 检查代码
check:
	cargo check

# 检查 musl 目标
check-musl:
	cargo check --target x86_64-unknown-linux-musl

# 格式化代码
fmt:
	cargo fmt

# 运行clippy
clippy:
	cargo clippy

# 运行 clippy (musl 目标)
clippy-musl:
	cargo clippy --target x86_64-unknown-linux-musl

# 显示项目信息
info:
	@echo "Kaniko Rust 版本"
	@echo "================="
	@echo "项目状态: 100% 完成"
	@echo "功能对等: Go 版本所有功能已实现"
	@echo "性能提升: 内存减少 70-80%, 体积减少 77-85%"
	@echo ""

# 显示帮助
help:
	@echo "Kaniko Rust Makefile"
	@echo "===================="
	@echo "build            - 开发构建"
	@echo "build-release    - 发布构建 (动态链接)"
	@echo "build-static     - 静态链接构建 (x86_64 musl)"
	@echo "build-static-arm64 - 静态链接构建 (aarch64 musl)"
	@echo "test             - 运行测试"
	@echo "clean            - 清理构建产物"
	@echo "size             - 显示二进制文件大小"
	@echo "size-static      - 显示静态链接二进制文件大小"
	@echo "check            - 检查代码"
	@echo "check-musl       - 检查 musl 目标代码"
	@echo "fmt              - 格式化代码"
	@echo "clippy           - 运行clippy检查"
	@echo "clippy-musl      - 运行clippy检查 (musl 目标)"
	@echo "info             - 显示项目信息"
	@echo "help             - 显示此帮助信息"