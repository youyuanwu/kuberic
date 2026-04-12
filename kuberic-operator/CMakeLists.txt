add_custom_target(kuberic-operator-image ALL
    COMMAND docker build -t localhost/kuberic-operator
        -f kuberic-operator/deploy/Dockerfile .
    COMMAND ${KIND_EXE} load docker-image localhost/kuberic-operator:latest --name kind
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    DEPENDS build_rust_bins
)

add_custom_target(kuberic-operator-deploy
    COMMAND kubectl apply -f kuberic-operator/deploy/deployment.yaml
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

add_custom_target(kuberic-operator-delete
    COMMAND kubectl delete -f kuberic-operator/deploy/deployment.yaml
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)
