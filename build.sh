#!/bin/bash

git pull 
make build-release
rm /usr/local/bin/kaniko-cli
mv target/release/kaniko-cli /usr/local/bin

echo "build test..."
kaniko-cli --force --sandbox --dockerfile Dockerfile --no-push --destination test.tar


RUST_LOG_LEVEL=debug kaniko-cli --force --sandbox --dockerfile Dockerfile --destination registry-intl.cn-shanghai.aliyuncs.com/mirror_library/alpine:test