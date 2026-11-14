#!/bin/sh
set -eu

if [ "$1" != "--kuberic-test-control-root" ]; then
    echo "missing owned qualification directory" >&2
    exit 2
fi
fault_directory=$2
shift 2

if [ -f "$fault_directory/copy-$Fabric_NodeIPOrFQDN" ]; then
    : > "$Fabric_Folder_App_Work/local-copy.block"
fi

exec "${0%/*}/native/hosting-local-app" "$@"
