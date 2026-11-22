#!/usr/bin/env bash
# Print the embedded locale file produced by this Cargo build.
set -euo pipefail

locale_file=$(
    cargo build --message-format=json "$@" |
        jq -ers '
            [.[] |
                select(.reason == "build-script-executed") |
                select(.package_id | split("#")[0] | endswith("/src/uucore")) |
                .out_dir] |
            unique |
            if length == 1 then .[0] + "/embedded_locales.rs"
            else error("expected one uucore build-script output")
            end
        '
)
test -f "$locale_file"
printf '%s\n' "$locale_file"
