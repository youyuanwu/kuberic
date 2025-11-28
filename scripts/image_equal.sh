#!/bin/bash

# returns 0 if images on host and kind are equal, 1 otherwise

KIND_EXE=${1}
IMAGE_NAME=${2}

HOST_IMAGE_ID=$(docker images --format "{{.ID}}" ${IMAGE_NAME}:latest)
# Get hash in Kind cluster
KIND_IMAGE_ID=$(docker exec kind-control-plane crictl images -o json | \
    jq -r ".images[] | select(.repoTags[]? | contains(\"${IMAGE_NAME}:latest\")) | .id" | \
    cut -d':' -f2 | head -c 12)

if [ "${HOST_IMAGE_ID}" == "${KIND_IMAGE_ID}" ]; then
    echo "Images are equal: ${IMAGE_NAME}"
    exit 0
else
    echo "Images are different: ${IMAGE_NAME} (host: ${HOST_IMAGE_ID} vs kind: ${KIND_IMAGE_ID})"
    exit 1
fi