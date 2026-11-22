#!/bin/sh
# User-local installation flow inspired by zoxide's official install.sh:
# https://github.com/ajeetdsouza/zoxide/blob/2f4bcebaa62a44742280172c9b4c2753266716d0/install.sh
# This implementation uses only the shell and standard Linux/macOS utilities.

err() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || err "need '$1' (command not found)"
}

usage() {
    printf '%s\n' \
        'SolaScope Linux/macOS installer' \
        'Usage: sh install.sh [OPTIONS]' \
        '  --version VERSION    Published version, optionally prefixed with v (default: latest stable)' \
        "  --bin-dir DIRECTORY  Absolute binary directory (default: ${HOME}/.local/bin)" \
        '  --arch TARGET        Override the detected Rust target; does not add release assets' \
        '  --archive FILE       Install a local, correctly named .tar.gz without network access' \
        '  --checksum FILE      Local SHA256SUMS for --archive' \
        '  --sha256 HASH        Explicit expected archive SHA-256 for --archive' \
        '  -h, --help           Show this help' \
        'No sudo, build tools, shell-profile edits, or model downloads are performed.'
}

valid_version() (
    case "$1" in '' | *[!0-9A-Za-z.+-]*) exit 1 ;; esac
    [ "${#1}" -le 128 ] || exit 1
    core=$1
    case "$core" in
    *+*)
        build=${core#*+}
        core=${core%%+*}
        case "$build" in '' | *[!0-9A-Za-z.-]* | .* | *. | *..*) exit 1 ;; esac
        ;;
    esac
    case "$core" in
    *-*)
        pre=${core#*-}
        core=${core%%-*}
        case "$pre" in '' | .* | *. | *..*) exit 1 ;; esac
        IFS=.
        # Split SemVer identifiers only on dots; globbing is disabled by main.
        # shellcheck disable=SC2086
        set -- $pre
        for part; do
            case "$part" in
            *[!0-9]*) ;;
            0[0-9]*) exit 1 ;;
            *) ;;
            esac
        done
        ;;
    esac
    case "$core" in .* | *. | *..*) exit 1 ;; esac
    IFS=.
    # shellcheck disable=SC2086 # Intentional dot splitting with globbing disabled.
    set -- $core
    [ "$#" -eq 3 ] || exit 1
    for part; do
        case "$part" in '' | *[!0-9]* | 0[0-9]*) exit 1 ;; esac
    done
)

valid_hash() {
    [ "${#1}" -eq 64 ] || return 1
    case "$1" in *[!0-9a-fA-F]*) return 1 ;; esac
}

get_architecture() (
    os=$(uname -s) || exit 1
    cpu=$(uname -m) || exit 1
    case "$os" in
    Linux)
        case "$cpu" in x86_64 | amd64) ;; *) err "Linux release packages currently require x86_64 glibc; detected $cpu" ;; esac
        command -v getconf >/dev/null 2>&1 || err 'getconf is required to detect the Linux libc/userland'
        libc=$(getconf GNU_LIBC_VERSION 2>/dev/null) || err 'Linux musl/Alpine is not supported by the GNU release package'
        case "$libc" in 'glibc '*) ;; *) err 'a glibc Linux userland is required' ;; esac
        [ "$(getconf LONG_BIT)" = 64 ] || err 'a 64-bit Linux userland is required'
        printf '%s\n' x86_64-unknown-linux-gnu
        ;;
    Darwin)
        # Prefer native Apple Silicon packages when invoked from Rosetta.
        if [ "$cpu" = x86_64 ] && [ "$(sysctl -in sysctl.proc_translated 2>/dev/null || :)" = 1 ]; then
            cpu=arm64
        fi
        case "$cpu" in
        x86_64) printf '%s\n' x86_64-apple-darwin ;;
        arm64 | aarch64) printf '%s%s\n' aa rch64-apple-darwin ;;
        *) err "unsupported macOS CPU: $cpu" ;;
        esac
        ;;
    *) err "unsupported OS: $os; use install.ps1 on Windows" ;;
    esac
)

download() {
    case "$1" in https://github.com/wchiway/SolaScope/releases/*) ;; *) err 'unexpected download URL' ;; esac
    if command -v curl >/dev/null 2>&1; then
        curl --fail --location --silent --show-error --proto '=https' --proto-redir '=https' \
            --connect-timeout 15 --max-time 120 --retry 2 --max-redirs 5 \
            --output "$2" "$1" || err "download failed: $1 (check network access and whether this platform asset is published)"
    elif command -v wget >/dev/null 2>&1; then
        wget --quiet --https-only --timeout=30 --tries=3 --max-redirect=5 \
            --output-document="$2" "$1" || err "download failed: $1 (check network access and whether this platform asset is published)"
    else
        err 'online installation needs curl or wget; use --archive for offline installation'
    fi
}

hash_file() (
    if command -v sha256sum >/dev/null 2>&1; then
        result=$(sha256sum <"$1") || exit 1
    elif command -v shasum >/dev/null 2>&1; then
        result=$(shasum -a 256 <"$1") || exit 1
    else
        err 'need sha256sum (Linux) or shasum (macOS)'
    fi
    printf '%s\n' "${result%% *}"
)

select_checksum() (
    selected=
    while IFS= read -r line || [ -n "$line" ]; do
        [ -n "$line" ] || continue
        digest=${line%% *}
        valid_hash "$digest" || err 'malformed SHA256SUMS hash'
        case "$line" in "$digest  "*) name=${line#"$digest  "} ;; *) err 'malformed SHA256SUMS line' ;; esac
        case "$name" in '' | *[!0-9A-Za-z.+_-]*) err 'unsafe SHA256SUMS filename' ;; esac
        case "$name" in
        solascope-*"-$2.tar.gz")
            [ -z "$selected" ] || err "multiple checksum entries for $2"
            candidate=${name#solascope-}
            candidate=${candidate%"-$2.tar.gz"}
            valid_version "$candidate" || err 'invalid version in archive checksum'
            [ -z "$3" ] || [ "$candidate" = "$3" ] || err 'checksum version does not match requested version'
            selected="$digest $candidate"
            ;;
        esac
    done <"$1"
    [ -n "$selected" ] || err "no published archive checksum for $2"
    printf '%s\n' "$selected"
)

validate_archive() (
    # Listing is inspected before any data is extracted. Only regular, public
    # entries with exact names are accepted; links, duplicates and PAX renames
    # cannot redirect the two explicit stdout extractions below.
    tar -tzf "$1" >"$2/members" || err 'could not list the archive'
    printf '%s\n' solascope sas LICENSE README.md README.zh-CN.md \
        docs/configuration.md docs/mcp.md docs/architecture.md docs/benchmarks.md \
        docs/implementation-status.md benchmarks/README.md \
        docs/assets/solascope-overview.png docs/assets/solascope-search-flow.png \
        >"$2/expected"
    # v0.1.0 predates bundled installers and installation/release guides.
    if [ "$3" != 0.1.0 ]; then
        printf '%s\n' install.sh docs/installation.md docs/releases.md >>"$2/expected"
    fi
    sort "$2/members" >"$2/members-sorted" || exit 1
    sort "$2/expected" >"$2/expected-sorted" || exit 1
    cmp -s "$2/members-sorted" "$2/expected-sorted" || err 'archive has missing, duplicate, unsafe, or unexpected entries'
    tar -tvzf "$1" >"$2/types" || err 'could not inspect archive entry types'
    while IFS= read -r entry; do
        case "$entry" in -*) ;; *) err 'archive contains a link, directory, or other non-regular entry' ;; esac
    done <"$2/types"
)

version_output() (
    executable=$1
    output=$2
    worker=
    timer=
    # ShellCheck does not follow this nested function's EXIT trap.
    # shellcheck disable=SC2317
    version_cleanup() {
        if [ -n "$timer" ]; then
            kill -TERM "$timer" 2>/dev/null || :
            wait "$timer" 2>/dev/null || :
        fi
        if [ -n "$worker" ]; then
            kill -TERM "$worker" 2>/dev/null || :
            wait "$worker" 2>/dev/null || :
        fi
    }
    trap version_cleanup 0
    trap 'exit 1' HUP INT TERM
    mkfifo "$output.events" || return 1
    (
        sleeper=
        trap 'if [ -n "$sleeper" ]; then kill -KILL "$sleeper" 2>/dev/null || :; wait "$sleeper" 2>/dev/null || :; fi' 0
        trap 'exit 0' HUP INT TERM
        sleep 30 &
        sleeper=$!
        printf '%s\n' ready
        wait "$sleeper" || exit 1
        sleeper=
        # The timer only reports an event; it never signals a native PID.
        printf '%s\n' timeout
    ) >"$output.events" 2>/dev/null &
    timer=$!
    # Open the event channel before starting any short-lived native process.
    # Keep it open across both reads: macOS can interrupt a FIFO open on SIGCHLD.
    exec 3<"$output.events"
    IFS= read -r event <&3 || return 1
    [ "$event" = ready ] || return 1
    (
        child=
        trap 'if [ -n "$child" ]; then kill -KILL "$child" 2>/dev/null || :; wait "$child" 2>/dev/null || :; fi' 0
        trap 'exit 1' HUP INT TERM
        (
            # Bound stdout and stderr as well as the native running time.
            ulimit -f 8
            exec "$executable" --version
        ) >"$output" 2>"$output.stderr" 3<&- &
        child=$!
        status=0
        wait "$child" || status=$?
        child=
        printf 'done %s\n' "$status"
    ) >"$output.events" 2>/dev/null 3<&- &
    worker=$!
    IFS= read -r event <&3 || return 1
    case "$event" in
    'done '*) status=${event#done } ;;
    timeout) err 'binary --version timed out' ;;
    *) return 1 ;;
    esac
    kill -TERM "$timer" 2>/dev/null || :
    wait "$timer" 2>/dev/null || :
    timer=
    wait "$worker" || return 1
    worker=
    exec 3<&-
    rm -f "$output.events"
    [ "$status" -eq 0 ] || return "$status"
    IFS= read -r text <"$output" || return 1
    printf '%s\n' "$text" >"$output.expected"
    cmp -s "$output" "$output.expected" || err 'binary --version must return exactly one line'
    printf '%s\n' "$text"
)

cleanup() {
    cleanup_status=$1
    trap - 0 HUP INT TERM
    restore_failed=0
    if [ "$complete" -eq 0 ] && [ -n "$stage" ]; then
        if [ "$published_solascope" -eq 1 ]; then
            if [ "$had_solascope" -eq 1 ]; then
                mv -f "$stage/old-solascope" "$bin_dir/solascope" || restore_failed=1
            else
                rm -f "$bin_dir/solascope" || restore_failed=1
            fi
        fi
        if [ "$published_sas" -eq 1 ]; then
            if [ "$had_sas" -eq 1 ]; then
                mv -f "$stage/old-sas" "$bin_dir/sas" || restore_failed=1
            else
                rm -f "$bin_dir/sas" || restore_failed=1
            fi
        fi
    fi
    if [ "$restore_failed" -eq 1 ]; then
        printf 'Error: rollback failed; preserve backups in %s for recovery.\n' "$stage" >&2
        cleanup_status=1
    elif [ -n "$stage" ]; then
        rm -rf "$stage" || printf 'Warning: could not remove staging directory %s\n' "$stage" >&2
    fi
    if [ -n "$work" ]; then
        rm -rf "$work" || printf 'Warning: could not remove download directory %s\n' "$work" >&2
    fi
    if [ "$locked" -eq 1 ]; then
        rmdir "$bin_dir/.solascope-install.lock" || printf 'Warning: could not remove install lock\n' >&2
    fi
    exit "$cleanup_status"
}

main() (
    set -eu
    set -f
    umask 077
    LC_ALL=C
    export LC_ALL
    # These variables can change tar/gzip semantics, including extraction flags.
    unset TAR_OPTIONS TAPE GZIP GZIP_OPT
    version=
    bin_dir=${HOME:?HOME must be set}/.local/bin
    arch=
    archive=
    checksum=
    expected_hash=
    while [ "$#" -gt 0 ]; do
        option=$1
        case "$option" in
        -h | --help)
            usage
            exit 0
            ;;
        --version | --bin-dir | --arch | --archive | --checksum | --sha256)
            if [ "$#" -lt 2 ] || [ -z "$2" ]; then err "missing value for $option"; fi
            value=$2
            shift 2
            ;;
        --version=* | --bin-dir=* | --arch=* | --archive=* | --checksum=* | --sha256=*)
            value=${option#*=}
            option=${option%%=*}
            [ -n "$value" ] || err "missing value for $option"
            shift
            ;;
        *) err "unknown option: $option (see --help)" ;;
        esac
        case "$option" in
        --version)
            version=${value#v}
            valid_version "$version" || err 'invalid SemVer version'
            ;;
        --bin-dir) bin_dir=$value ;;
        --arch) arch=$value ;;
        --archive) archive=$value ;;
        --checksum) checksum=$value ;;
        --sha256)
            expected_hash=$value
            valid_hash "$expected_hash" || err '--sha256 needs 64 hexadecimal characters'
            ;;
        esac
    done
    [ -n "$arch" ] || arch=$(get_architecture) || exit 1
    case "$arch" in x86_64-unknown-linux-gnu | x86_64-apple-darwin | aa"rch64-apple-darwin") ;; *) err "unsupported release target: $arch" ;; esac
    case "$bin_dir" in /*) ;; *) err '--bin-dir must be an absolute path' ;; esac
    case "$bin_dir" in *:*) err '--bin-dir must not contain a PATH separator' ;; esac
    case "$bin_dir$archive$checksum${TMPDIR-}" in *'
'*) err 'paths must not contain newlines' ;; esac
    while [ "$bin_dir" != / ] && [ "${bin_dir%/}" != "$bin_dir" ]; do bin_dir=${bin_dir%/}; done
    case "$bin_dir/" in */./* | */../*) err '--bin-dir must not contain dot segments' ;; esac
    case "$archive" in '' | /*) ;; *) archive=$PWD/$archive ;; esac
    case "$checksum" in '' | /*) ;; *) checksum=$PWD/$checksum ;; esac
    case "${TMPDIR:-/tmp}" in /*) ;; *) err 'TMPDIR must be absolute' ;; esac
    for utility in uname mktemp mkfifo mkdir rmdir dirname cp mv rm chmod tar gzip sort cmp tr sleep; do
        need_cmd "$utility"
    done
    if [ -n "$archive" ]; then
        [ -n "$checksum$expected_hash" ] || err '--archive requires --checksum or --sha256'
        if [ ! -f "$archive" ] || [ ! -r "$archive" ]; then err 'archive is not a readable regular file'; fi
    else
        [ -z "$checksum$expected_hash" ] || err '--checksum and --sha256 require --archive'
    fi
    work=
    stage=
    locked=0
    complete=0
    had_solascope=0
    had_sas=0
    published_solascope=0
    published_sas=0
    trap 'cleanup "$?"' 0
    trap 'exit 1' HUP INT TERM
    work=$(mktemp -d "${TMPDIR:-/tmp}/solascope-download.XXXXXXXX") || err 'could not create a temporary directory'
    if [ -n "$archive" ]; then
        asset=${archive##*/}
        case "$asset" in solascope-*"-$arch.tar.gz") ;; *) err 'offline archive filename must match solascope-<version>-<target>.tar.gz' ;; esac
        resolved=${asset#solascope-}
        resolved=${resolved%"-$arch.tar.gz"}
        valid_version "$resolved" || err 'invalid offline archive version'
        [ -z "$version" ] || [ "$version" = "$resolved" ] || err 'offline archive version does not match --version'
        version=$resolved
        cp "$archive" "$work/package.tar.gz" || err 'could not copy offline archive'
        if [ -n "$checksum" ]; then
            cp "$checksum" "$work/SHA256SUMS" || err 'could not copy offline checksums'
        fi
    else
        base=https://github.com/wchiway/SolaScope/releases
        if [ -n "$version" ]; then
            download "$base/download/v$version/SHA256SUMS" "$work/SHA256SUMS"
        else
            # Resolve latest once via its checksum asset, avoiding a JSON parser.
            # Both the archive name and its expected digest are frozen here.
            download "$base/latest/download/SHA256SUMS" "$work/SHA256SUMS"
        fi
    fi
    if [ -f "$work/SHA256SUMS" ]; then
        selected=$(select_checksum "$work/SHA256SUMS" "$arch" "$version") || exit 1
        selected_hash=${selected%% *}
        version=${selected#* }
        if [ -n "$expected_hash" ]; then
            normalized=$(printf '%s' "$expected_hash" | tr 'A-F' 'a-f')
            selected_hash=$(printf '%s' "$selected_hash" | tr 'A-F' 'a-f')
            [ "$normalized" = "$selected_hash" ] || err 'checksum sources disagree'
        fi
        expected_hash=$selected_hash
    fi
    if [ -z "$archive" ]; then
        asset=solascope-$version-$arch.tar.gz
        download "$base/download/v$version/$asset" "$work/package.tar.gz"
    fi
    actual=$(hash_file "$work/package.tar.gz") || err 'could not compute archive SHA-256'
    expected_hash=$(printf '%s' "$expected_hash" | tr 'A-F' 'a-f')
    [ "$actual" = "$expected_hash" ] || err 'archive SHA-256 mismatch; nothing was installed'
    validate_archive "$work/package.tar.gz" "$work" "$version" || exit 1
    # Extract only the two fixed names into a fresh directory, never unpack
    # arbitrary archive paths or run tools with the archive as their cwd.
    for name in solascope sas; do
        (
            ulimit -f 131072
            tar -xOzf "$work/package.tar.gz" "$name" >"$work/$name"
        ) || err "could not extract $name"
    done
    # Reject symlink components instead of following them into another location.
    cursor=$bin_dir
    while [ "$cursor" != / ] && [ -n "$cursor" ]; do
        [ ! -L "$cursor" ] || err 'binary directory must not contain symlink components'
        cursor=$(dirname "$cursor")
    done
    mkdir -p "$bin_dir" || err 'binary directory is not writable; choose --bin-dir (no sudo is used)'
    bin_dir=$(CDPATH='' cd -P "$bin_dir" && pwd) || exit 1
    if [ "$bin_dir" = / ] || [ "$bin_dir" = "$HOME" ]; then err 'refusing a root or home directory as --bin-dir'; fi
    mkdir "$bin_dir/.solascope-install.lock" 2>/dev/null || err "another installer may be active; inspect $bin_dir/.solascope-install.lock before retrying"
    locked=1
    stage=$(mktemp -d "$bin_dir/.solascope-install.XXXXXXXX") || err 'could not stage the installation'
    for name in solascope sas; do
        [ ! -L "$bin_dir/$name" ] || err "refusing to replace symlink: $bin_dir/$name"
        if [ -e "$bin_dir/$name" ]; then
            [ -f "$bin_dir/$name" ] || err "refusing to replace a non-regular file: $name"
            cp -p "$bin_dir/$name" "$stage/old-$name" || err "could not back up $name"
            case "$name" in solascope) had_solascope=1 ;; sas) had_sas=1 ;; esac
        fi
        cp "$work/$name" "$stage/$name" || err "could not stage $name"
        chmod 755 "$stage/$name" || err "could not make $name executable"
        result=$(version_output "$stage/$name" "$work/version-$name") || err "$name cannot run on this system; existing installation was preserved"
        [ "$result" = "solascope $version" ] || err "$name version does not match the selected release"
    done
    # Two flat files cannot be replaced as one atomic operation. Backups and a
    # destination lock permit rollback of both names if either replacement fails.
    published_solascope=1
    mv -f "$stage/solascope" "$bin_dir/solascope" || err 'could not install solascope'
    published_sas=1
    mv -f "$stage/sas" "$bin_dir/sas" || err 'could not install sas'
    complete=1
    printf 'Installed SolaScope %s (%s) into %s\n' "$version" "$arch" "$bin_dir"
    case ":${PATH-}:" in
    *":$bin_dir:"*) ;;
    *) printf 'Add %s to PATH in your shell configuration to use solascope and sas.\n' "$bin_dir" ;;
    esac
    printf '%s\n' 'Existing configuration and shell profiles were preserved. No model runtimes were installed.'
)

# Do not execute a partially downloaded script.
{
    main "$@"
}
