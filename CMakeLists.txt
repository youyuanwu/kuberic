cmake_minimum_required(VERSION 3.16)

project(xedio LANGUAGES)

if(NOT DEFINED ENV{XEDIO_CI})
    # find kind from bashrc alias
    execute_process(
        COMMAND bash -i -c "alias kind | sed \"s/alias kind='//\" | sed \"s/'$//\""
        OUTPUT_VARIABLE KIND_PATH
        OUTPUT_STRIP_TRAILING_WHITESPACE
        COMMAND_ERROR_IS_FATAL ANY
    )
    message(STATUS "Kind path: ${KIND_PATH}")
    execute_process(
        COMMAND ${KIND_PATH} version
        COMMAND_ERROR_IS_FATAL ANY
    )
    set(KIND_EXE ${KIND_PATH})
else()
    find_program(KIND_EXE kind REQUIRED)
endif()

add_custom_target(create-kind-cluster
    COMMAND ${KIND_EXE} create cluster --name kind 
        --config=deploy/kind-config.yaml
        --kubeconfig ~/.kube/config
    COMMAND ${KIND_EXE} export kubeconfig --name kind --kubeconfig ~/.kube/config
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

add_custom_target(delete-kind-cluster
    COMMAND ${KIND_EXE} delete cluster --name kind
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

add_custom_target(build_rust_bins
    COMMAND cargo build --bins --workspace
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    COMMENT "Building Rust workspace with Cargo"
    VERBATIM
)

add_subdirectory(kuberic-operator)
add_subdirectory(examples/kvstore)