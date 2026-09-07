#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/../.." && pwd)
lock_file="$script_dir/source.lock"
fixture_dir="$repo_root/crates/analyzer/tests/fixtures/digital_sid_oracle/v1"
mode=check
provided_archive=
output_dir=

usage() {
    echo "usage: $0 [--check|--write|--output-dir PATH] [--archive PATH]" >&2
}

while (($# > 0)); do
    case "$1" in
        --check)
            mode=check
            shift
            ;;
        --write)
            mode=write
            shift
            ;;
        --output-dir)
            if (($# < 2)); then
                usage
                exit 2
            fi
            mode=output
            output_dir=$2
            shift 2
            ;;
        --archive)
            if (($# < 2)); then
                usage
                exit 2
            fi
            provided_archive=$2
            shift 2
            ;;
        *)
            usage
            exit 2
            ;;
    esac
done

lock_value() {
    local key=$1
    sed -n "s/^${key}=//p" "$lock_file"
}

archive_url=$(lock_value archive_url)
archive_sha256=$(lock_value archive_sha256)
source_directory=$(lock_value source_directory)
source_revision=$(lock_value revision)
source_version=$(lock_value version)
source_tag=$(lock_value tag)
source_repository=$(lock_value repository_url)
locked_compiler_id=$(lock_value compiler_id)
locked_library_cxxflags=$(lock_value library_cxxflags)
locked_library_cppflags=$(lock_value library_cppflags)
locked_library_ldflags=$(lock_value library_ldflags)
locked_generator_cxxflags=$(lock_value generator_cxxflags)

if [[ -z "$archive_url" || -z "$archive_sha256" || -z "$source_directory" || -z "$source_revision" || -z "$source_version" || -z "$source_tag" || -z "$source_repository" || -z "$locked_compiler_id" || -z "$locked_library_cxxflags" || -z "$locked_library_cppflags" || -z "$locked_library_ldflags" || -z "$locked_generator_cxxflags" ]]; then
    echo "source.lock is incomplete" >&2
    exit 1
fi

compiler=${CXX:-c++}
sanitized_env=(env -i "PATH=$PATH" LANG=C LC_ALL=C)
compiler_id=$("${sanitized_env[@]}" "$compiler" -dumpfullversion -dumpversion)
if [[ "$compiler_id" != "$locked_compiler_id" ]]; then
    echo "compiler mismatch: source.lock requires $locked_compiler_id, found $compiler_id" >&2
    exit 1
fi
library_cppflags=
library_ldflags=
if [[ "$locked_library_cppflags" != "<empty>" || "$locked_library_ldflags" != "<empty>" ]]; then
    echo "source.lock uses an unsupported non-empty preprocessor or linker flag set" >&2
    exit 1
fi
read -r -a generator_cxxflags <<< "$locked_generator_cxxflags"
if ((${#generator_cxxflags[@]} == 0)); then
    echo "source.lock has no generator compiler flags" >&2
    exit 1
fi

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/sid-oracle.XXXXXXXX")
case "$work_dir" in
    "${TMPDIR:-/tmp}"/sid-oracle.*) ;;
    *)
        echo "refusing unexpected temporary directory: $work_dir" >&2
        exit 1
        ;;
esac

cleanup() {
    if [[ "${SID_ORACLE_KEEP_WORK:-0}" == 1 ]]; then
        echo "retained oracle work directory: $work_dir" >&2
        return
    fi
    if [[ -n "${work_dir:-}" && -d "$work_dir" ]]; then
        rm -rf -- "$work_dir"
    fi
}
trap cleanup EXIT

archive="$work_dir/libresidfp.tar.gz"
if [[ -n "$provided_archive" ]]; then
    cp -- "$provided_archive" "$archive"
else
    curl -L --fail --silent --show-error -o "$archive" "$archive_url"
fi

printf '%s  %s\n' "$archive_sha256" "$archive" | sha256sum --check --status
tar -xzf "$archive" -C "$work_dir"
source_root="$work_dir/$source_directory"
if [[ ! -d "$source_root/src" ]]; then
    echo "verified archive lacks expected source directory: $source_directory" >&2
    exit 1
fi

(
    cd -- "$source_root"
    "${sanitized_env[@]}" \
        CXX="$compiler" \
        CXXFLAGS="$locked_library_cxxflags" \
        CPPFLAGS="$library_cppflags" \
        LDFLAGS="$library_ldflags" \
        ./configure --quiet --disable-shared --enable-static
    "${sanitized_env[@]}" \
        make --silent -j2 src/libresidfp.la \
        CXX="$compiler" \
        CXXFLAGS="$locked_library_cxxflags" \
        CPPFLAGS="$library_cppflags" \
        LDFLAGS="$library_ldflags"
)

generator_revision=$(sha256sum "$script_dir/generate.cpp" | awk '{print $1}')
build_flags="libresidfp CXXFLAGS=$locked_library_cxxflags CPPFLAGS=$locked_library_cppflags LDFLAGS=$locked_library_ldflags; generator CXXFLAGS=$locked_generator_cxxflags"
generator="$work_dir/digital-sid-oracle"

"${sanitized_env[@]}" "$compiler" "${generator_cxxflags[@]}" \
    -I"$source_root/src" \
    -DORACLE_SOURCE_VERSION="\"$source_version\"" \
    -DORACLE_SOURCE_REVISION="\"$source_revision\"" \
    -DORACLE_SOURCE_SHA256="\"$archive_sha256\"" \
    -DORACLE_SOURCE_REPOSITORY="\"$source_repository\"" \
    -DORACLE_SOURCE_TAG="\"$source_tag\"" \
    -DORACLE_GENERATOR_REVISION="\"$generator_revision\"" \
    -DORACLE_BUILD_FLAGS="\"$build_flags\"" \
    -DORACLE_COMPILER_ID="\"GCC $compiler_id\"" \
    "$script_dir/generate.cpp" \
    "$source_root/src/.libs/libresidfp.a" \
    -pthread \
    -o "$generator"

generated_a="$work_dir/generated-a"
generated_b="$work_dir/generated-b"
mkdir -p -- "$generated_a" "$generated_b"
"$generator" "$generated_a"
"$generator" "$generated_b"
diff -ru -- "$generated_a" "$generated_b"

if [[ "$mode" != output ]]; then
    validation_root="$work_dir/validation-root"
    mkdir -p -- "$validation_root"
    cp -- "$fixture_dir/manifest.json" "$validation_root/manifest.json"
    cp -- "$fixture_dir/smoke.oracle.json" "$validation_root/smoke.oracle.json"
    while IFS= read -r relative; do
        if [[ -z "$relative" || "$relative" = /* || "$relative" = *..* ]]; then
            echo "generator emitted unsafe file name: $relative" >&2
            exit 1
        fi
        cp -- "$generated_a/$relative" "$validation_root/$relative"
    done < "$generated_a/generated-files.txt"
    SID_ORACLE_FIXTURE_ROOT="$validation_root" \
        cargo test --quiet --manifest-path "$repo_root/Cargo.toml" \
        -p sid-analyzer --test digital_sid_oracle generated_oracle_vectors_match_the_reviewed_policy
else
    if [[ -e "$output_dir" ]]; then
        echo "refusing to overwrite output directory: $output_dir" >&2
        exit 1
    fi
    mkdir -p -- "$output_dir"
fi

while IFS= read -r relative; do
    if [[ -z "$relative" || "$relative" = /* || "$relative" = *..* ]]; then
        echo "generator emitted unsafe file name: $relative" >&2
        exit 1
    fi
    source_file="$generated_a/$relative"
    destination_root=$fixture_dir
    if [[ "$mode" == output ]]; then
        destination_root=$output_dir
    fi
    destination="$destination_root/$relative"
    if [[ ! -f "$source_file" ]]; then
        echo "generator listed missing file: $relative" >&2
        exit 1
    fi
    if [[ "$mode" == check ]]; then
        if [[ ! -f "$destination" ]]; then
            echo "committed oracle file is missing: $relative" >&2
            exit 1
        fi
        cmp --silent -- "$source_file" "$destination" || {
            echo "committed oracle file differs: $relative" >&2
            diff -u -- "$destination" "$source_file" || true
            exit 1
        }
    else
        mkdir -p -- "$(dirname -- "$destination")"
        staged="$destination.new"
        cp -- "$source_file" "$staged"
        mv -- "$staged" "$destination"
    fi
done < "$generated_a/generated-files.txt"

echo "libresidfp $source_version ($source_revision) oracle vectors: $mode ok"
