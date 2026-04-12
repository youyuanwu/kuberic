add_custom_target(kubelicate-operator-image ALL
    COMMAND docker build -t localhost/kubelicate-operator
        -f kubelicate-operator/deploy/Dockerfile .
    COMMAND ${KIND_EXE} load docker-image localhost/kubelicate-operator:latest --name kind
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    DEPENDS build_rust_bins
)

add_custom_target(kubelicate-operator-deploy
    COMMAND kubectl apply -f kubelicate-operator/deploy/deployment.yaml
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

add_custom_target(kubelicate-operator-delete
    COMMAND kubectl delete -f kubelicate-operator/deploy/deployment.yaml
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)
