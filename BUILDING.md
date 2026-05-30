# 构建指南

## 系统要求

- Rust 1.70+
- Cargo
- 对于 Linux 构建：Docker（推荐）

## 构建选项

### 1. 本地开发构建

```bash
# 开发构建
make build

# 发布构建
make build-release

# 运行测试
make test
```

### 2. 多平台构建

#### macOS (本地)
```bash
# 构建 macOS 版本
cargo build --release
# 输出: target/release/kaniko-cli
```

#### Linux (使用 Docker)

**方法 1：使用提供的 Dockerfile**
```bash
# 构建 Linux 版本
docker build -f Dockerfile.linux -t kaniko-rust-linux .

# 提取二进制文件
docker run --rm -v $(pwd)/release:/output kaniko-rust-linux cp /kaniko-cli /output/kaniko-cli-linux-amd64
```

**方法 2：手动构建**
```bash
# 在 Linux 环境中
cargo build --release
# 输出: target/release/kaniko-cli
```

## 发布流程

### 使用发布脚本

```bash
# 运行发布脚本
./scripts/release.sh

# 查看发布文件
ls -la release/
```

发布脚本会：
- 构建 macOS 版本
- 提供 Linux 构建说明
- 创建发布目录和文件

### 手动发布

```bash
# 1. 构建
cargo build --release

# 2. 复制二进制文件
cp target/release/kaniko-cli release/kaniko-cli-$(uname -s)-$(uname -m)

# 3. 设置执行权限
chmod +x release/kaniko-cli-$(uname -s)-$(uname -m)
```

## 验证构建

```bash
# 检查二进制文件
ls -la target/release/kaniko-cli

# 运行帮助
./target/release/kaniko-cli --help

# 验证版本
./target/release/kaniko-cli --version
```

## 常见问题

### 1. 交叉编译问题

如果在 macOS 上交叉编译 Linux 版本遇到问题，建议使用 Docker 方法。

### 2. 依赖问题

确保所有依赖都已安装：
```bash
# 更新依赖
cargo update

# 检查依赖
cargo tree
```

### 3. 构建缓存

清理构建缓存：
```bash
make clean
```

## 平台支持

| 平台 | 架构 | 构建方法 | 状态 |
|------|------|----------|------|
| macOS | x86_64 | 本地 | ✅ |
| macOS | ARM64 | 本地 | ✅ |
| Linux | x86_64 | Docker | ✅ |
| Linux | ARM64 | Docker | ✅ |
| Windows | x86_64 | 交叉编译 | ⚠️ 未测试 |

## 性能指标

- **二进制大小**: 8-12MB (相比 Go 版本的 53MB)
- **内存使用**: 10-15MB (相比 Go 版本的 50MB)
- **构建时间**: 取决于硬件，通常在 1-3 分钟

## 发布文件命名

发布文件遵循以下命名约定：
```
kaniko-cli-{platform}-{arch}-{version}
```

例如：
- `kaniko-cli-darwin-amd64-0.1.0`
- `kaniko-cli-linux-amd64-0.1.0`