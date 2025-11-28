# Cannot make output dep because we don't know the src changed.

add_custom_target(xdata-app-image ALL
    COMMAND docker build -t localhost/xdata-app
        -f apps/xdata-app/deploy/Dockerfile .
    COMMAND ${KIND_EXE} load docker-image localhost/xdata-app:latest --name kind
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    DEPENDS build_rust_bins
)

add_custom_target(xdata-deploy
    COMMAND kubectl apply -f apps/xdata-operator/deploy/xdataapp-example.yaml
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

add_custom_target(xdata-delete
    COMMAND kubectl delete -f apps/xdata-operator/deploy/xdataapp-example.yaml
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)