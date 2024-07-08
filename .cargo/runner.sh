#!/bin/sh
set -e

user=$1
host=$2
path=$3
bin=$4
shift 4

target="$user@$host"

scp "$bin" "$target:$path"
ssh -t $target RUST_BACKTRACE=1 "/home/$user/$path/$(basename "$bin")" "$@"
