#!/bin/bash
set -x
run_podman_tests() {
    if ! command -v podman &> /dev/null; then
        echo "Podman is not installed"
        return 1
    fi

    echo "Running tests with Podman..."
    podman login --tls-verify=false --username devuser --password devpw http://${REGISTRY_ADDR}
    podman rmi hello-world
    podman pull hello-world
    podman tag hello-world ${REGISTRY_ADDR}/testing/hello:prod
    podman push --tls-verify=false ${REGISTRY_ADDR}/testing/hello:prod
}

run_docker_tests() {
    if ! command -v docker &> /dev/null; then
        echo "Docker is not installed"
        return 1
    fi

    echo "Running tests with Docker..."
    docker login --username devuser --password devpw http://${REGISTRY_ADDR}
    docker rmi hello-world
    docker pull hello-world
    docker tag hello-world ${REGISTRY_ADDR}/testing/hello:prod
    docker push ${REGISTRY_ADDR}/testing/hello:prod
}

export REGISTRY_ADDR=127.0.0.1:3000

if [ "x$PODMAN_IS_REMOTE" == "xtrue" ]; then
  export REGISTRY_ADDR=$(dig +short $(hostname)):3000
fi

echo "registry: ${REGISTRY_ADDR}"

cargo run --features="bin" &
CARGO_PID=$!

sleep 2

run_podman_tests

sleep 2

run_docker_tests

sleep 2

echo "Testing with curl..."
curl -v http://${REGISTRY_ADDR}/testing/hello

kill $CARGO_PID
