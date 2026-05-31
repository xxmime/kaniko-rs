#!/bin/bash

git pull 
make build-release
rm /usr/local/bin/kaniko-cli
mv target/release/kaniko-cli /usr/local/bin
