#!/usr/bin/env bash
# Downloads a pinned Auditstream CLI release archive from GitHub Releases,
# verifies its SHA-256 checksum before extraction, installs the binary into a
# private temporary directory, and exports it to GITHUB_PATH / GITHUB_OUTPUT.

set -euo pipefail
export LC_ALL=C

raw_version="${INPUT_VERSION:-}"
version="${raw_version#auditstream-v}"
if [[ "$version" == "$raw_version" ]]; then
  version="${raw_version#v}"
fi

if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*)?$ ]]; then
  echo "ERROR: version must be a pinned SemVer release (for example '0.1.0' or 'auditstream-v0.1.0'); got '${raw_version}'" >&2
  exit 1
fi

pinned_sha256=""
if [[ -n "${INPUT_SHA256:-}" ]]; then
  pinned_sha256="$(printf '%s' "$INPUT_SHA256" | tr '[:upper:]' '[:lower:]')"
  if [[ ! "$pinned_sha256" =~ ^[0-9a-f]{64}$ ]]; then
    echo "ERROR: sha256 input must be a 64-character hex digest (got '${INPUT_SHA256}')" >&2
    exit 1
  fi
fi

raw_os="${RUNNER_OS:-$(uname -s)}"
raw_arch="${RUNNER_ARCH:-$(uname -m)}"

case "$raw_os" in
  Linux|linux) os="linux" ;;
  macOS|Darwin|darwin) os="darwin" ;;
  Windows|windows|MINGW*|MSYS*|CYGWIN*) os="windows" ;;
  *)
    echo "ERROR: unsupported operating system '${raw_os}'" >&2
    exit 1
    ;;
esac

case "$raw_arch" in
  X64|x86_64|amd64) arch="x86_64" ;;
  ARM64|aarch64|arm64) arch="aarch64" ;;
  *)
    echo "ERROR: unsupported architecture '${raw_arch}'" >&2
    exit 1
    ;;
esac

case "${os}/${arch}" in
  linux/x86_64)   target="x86_64-unknown-linux-musl";  ext="tar.xz"; bin_name="auditstream" ;;
  linux/aarch64)  target="aarch64-unknown-linux-musl"; ext="tar.xz"; bin_name="auditstream" ;;
  darwin/x86_64)  target="x86_64-apple-darwin";        ext="tar.xz"; bin_name="auditstream" ;;
  darwin/aarch64) target="aarch64-apple-darwin";       ext="tar.xz"; bin_name="auditstream" ;;
  windows/x86_64) target="x86_64-pc-windows-msvc";     ext="zip";    bin_name="auditstream.exe" ;;
  *)
    echo "ERROR: no prebuilt auditstream release binary for ${os}/${arch}" >&2
    exit 1
    ;;
esac

compute_sha256() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum < "$file" | awk '{print $1}' | tr '[:upper:]' '[:lower:]'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 < "$file" | awk '{print $1}' | tr '[:upper:]' '[:lower:]'
  else
    echo "ERROR: neither sha256sum nor shasum is available to verify archive checksum" >&2
    exit 1
  fi
}

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

curl_args=(
  --proto '=https'
  --proto-redir '=https'
  --tlsv1.2
  --fail
  --silent
  --show-error
  --location
  --retry 3
  --retry-delay 2
  --connect-timeout 15
  --max-time 120
)

tag="auditstream-v${version}"
asset="auditstream-${target}.${ext}"
base_url="https://github.com/auditstream/auditstream/releases/download/${tag}"
archive_path="${tmp_dir}/${asset}"

curl "${curl_args[@]}" --output "$archive_path" "${base_url}/${asset}"

if [[ -n "$pinned_sha256" ]]; then
  expected_sha256="$pinned_sha256"
else
  checksum_path="${tmp_dir}/${asset}.sha256"
  curl "${curl_args[@]}" --output "$checksum_path" "${base_url}/${asset}.sha256"
  checksum_line="$(tr -d '\r' < "$checksum_path" | sed '/^[[:space:]]*$/d')"
  line_count="$(printf '%s\n' "$checksum_line" | wc -l | tr -d '[:space:]')"
  if [[ "$line_count" -ne 1 ]] || [[ ! "$checksum_line" =~ ^([0-9a-fA-F]{64})[[:space:]]+\*?([^[:space:]]+)$ ]]; then
    echo "ERROR: malformed checksum file ${asset}.sha256" >&2
    exit 1
  fi
  expected_sha256="$(printf '%s' "${BASH_REMATCH[1]}" | tr '[:upper:]' '[:lower:]')"
  recorded_name="${BASH_REMATCH[2]}"
  if [[ "$recorded_name" != "$asset" ]]; then
    echo "ERROR: checksum file ${asset}.sha256 references '${recorded_name}' instead of '${asset}'" >&2
    exit 1
  fi
fi

actual_sha256="$(compute_sha256 "$archive_path")"
if [[ ! "$actual_sha256" =~ ^[0-9a-f]{64}$ || "$actual_sha256" != "$expected_sha256" ]]; then
  echo "ERROR: SHA-256 checksum mismatch for ${asset}" >&2
  echo "  expected: ${expected_sha256}" >&2
  echo "  actual:   ${actual_sha256}" >&2
  exit 1
fi

extract_dir="${tmp_dir}/extract"
mkdir -p "$extract_dir"
if [[ "$ext" == "tar.xz" ]]; then
  tar -xJf "$archive_path" -C "$extract_dir"
  extracted_bin="${extract_dir}/auditstream-${target}/${bin_name}"
else
  unzip -q "$archive_path" -d "$extract_dir"
  if [[ -f "${extract_dir}/${bin_name}" && ! -L "${extract_dir}/${bin_name}" ]]; then
    extracted_bin="${extract_dir}/${bin_name}"
  else
    extracted_bin="${extract_dir}/auditstream-${target}/${bin_name}"
  fi
fi

if [[ ! -f "$extracted_bin" || -L "$extracted_bin" ]]; then
  echo "ERROR: expected binary '${bin_name}' not found as a regular file inside ${asset}" >&2
  exit 1
fi

if [[ -n "${RUNNER_TEMP:-}" ]]; then
  mkdir -p "$RUNNER_TEMP"
  install_dir="$(mktemp -d "${RUNNER_TEMP%/}/auditstream-${version}.XXXXXX")"
else
  install_dir="$(mktemp -d)"
fi

binary_path="${install_dir}/${bin_name}"
cp "$extracted_bin" "$binary_path"
chmod 0755 "$binary_path"

installed_version_line="$("$binary_path" --version)"
echo "Installed ${installed_version_line} (sha256:${actual_sha256}) at ${binary_path}"

path_entry="$install_dir"
output_bin_path="$binary_path"
if [[ "$os" == "windows" ]] && command -v cygpath >/dev/null 2>&1; then
  path_entry="$(cygpath -w "$install_dir")"
  output_bin_path="$(cygpath -w "$binary_path")"
fi

if [[ -n "${GITHUB_PATH:-}" ]]; then
  printf '%s\n' "$path_entry" >> "$GITHUB_PATH"
fi

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  {
    printf 'version=%s\n' "$version"
    printf 'binary-path=%s\n' "$output_bin_path"
    printf 'sha256=%s\n' "$actual_sha256"
  } >> "$GITHUB_OUTPUT"
fi
