#!/usr/bin/env bash
cd $(dirname ${BASH_SOURCE[0]})
docker buildx build -t agents-test .
