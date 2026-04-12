add_custom_target(kvstore-image ALL
    COMMAND docker build -t localhost/kvstore
        -f examples/kvstore/deploy/Dockerfile .
    COMMAND ${KIND_EXE} load docker-image localhost/kvstore:latest --name kind
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    DEPENDS build_rust_bins
)

add_custom_target(kvstore-deploy
    COMMAND kubectl apply -f examples/kvstore/deploy/kubelicateset.yaml
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

add_custom_target(kvstore-delete
    COMMAND kubectl delete -f examples/kvstore/deploy/kubelicateset.yaml
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)
