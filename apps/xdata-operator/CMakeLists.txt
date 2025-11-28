add_custom_target(xdata-operator-image ALL
    COMMAND docker build -t localhost/xdata-operator
        -f apps/xdata-operator/deploy/Dockerfile .
    COMMAND ${KIND_EXE} load docker-image localhost/xdata-operator:latest --name kind
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    DEPENDS build_rust_bins
)

add_custom_target(xdata-operator-deploy
    COMMAND kubectl apply -f apps/xdata-operator/deploy/xdataapp-crd.yaml
    COMMAND kubectl apply -f apps/xdata-operator/deploy/deployment.yaml
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

add_custom_target(xdata-operator-delete
    COMMAND kubectl delete -f apps/xdata-operator/deploy/deployment.yaml
    COMMAND kubectl delete -f apps/xdata-operator/deploy/xdataapp-crd.yaml
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)