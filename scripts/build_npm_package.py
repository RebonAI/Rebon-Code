"""Build npm-installable tarballs for Rebon platform and wrapper packages.

Usage:
    python scripts/build_npm_package.py --build
    python scripts/build_npm_package.py --binary target/release/rebon.exe
    python scripts/build_npm_package.py --kind all --binary target/release/rebon.exe
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
CARGO_TOML = REPO_ROOT / "Cargo.toml"
README_MD = REPO_ROOT / "README.md"
THIRD_PARTY_NOTICES = REPO_ROOT / "THIRD_PARTY_NOTICES.txt"
LICENSE_FILE = REPO_ROOT / "LICENSE"
NOTICE_FILE = REPO_ROOT / "NOTICE"
DEFAULT_DESCRIPTION = "Agent cli for coding and more."
# npm verifies the provenance statement trusted publishing signs against
# package.json: a repository.url that does not normalise to the repository
# the workflow ran in is a 422, and so is a missing one, which reads as "".
REPOSITORY_URL = "git+https://github.com/RebonAI/Rebon-Code.git"
HOMEPAGE_URL = "https://reboncode.ai"
DEFAULT_OUT_DIR = REPO_ROOT / "dist" / "npm"
DEFAULT_BIN_NAME = "rebon"
BOA_HELPER_NAME = "rebon-boa-helper"
# The Windows sandbox helper. Ships beside rebon.exe rather than installed
# separately. The caller's discovery order
# checks the running executable's own directory first precisely because that
# is where an npm install puts it.
SANDBOX_WIN_HELPER_NAME = "sandbox-win"
# Binaries a plugin ships beside `rebon`.
# The list is never written down: it is every `[[bin]]` target whose crate
# lives under `crates/plugins/`, read from `cargo metadata`. Unlike the sandbox
# helper these are required — each one is the whole of a feature the product
# advertises, so a package missing one is a broken package, not a smaller one.
PLUGIN_CRATES_DIR = REPO_ROOT / "crates" / "plugins"
NPM_ASSETS_ROOT = REPO_ROOT / "npm"
WRAPPER_PACKAGE_NAME = "@rebon/cli"
BARE_PACKAGE_NAME = "rebon"
BARE_ASSETS_ROOT = NPM_ASSETS_ROOT / "bare"
PLATFORM_PACKAGE_SCOPE = "@rebon"
WINDOWS_LAUNCHER_NAME = "rebon.js"
WINDOWS_PAYLOAD_NAME = "rebon.exe"
WINDOWS_MANAGED_LIB_NAME = "windows-managed.js"
WINDOWS_POSTINSTALL_NAME = "postinstall.js"
RIPGREP_BINARY_NAME = "rg"
RIPGREP_VERSION = "14.1.1"
WRAPPER_LAUNCHER = NPM_ASSETS_ROOT / "wrapper" / "bin" / "rebon.js"
NODE_ROOT = REPO_ROOT / "runtimes" / "node"
# The plugin plane's script trees, laid out next to the executable so
# rebon-plugin-supervisor's locate_host_script/locate_compose_loader find them
# in an install the way tests find them in a checkout. Test directories stay
# in the repo; the payload ships because the composition runtime resolves the
# vendored dsh packages from `compose-runtime/payload`.
PLANE_SCRIPT_SETS = (
    ("plugin-host", ("src", "package.json")),
    (
        "compose-runtime",
        ("src", "payload", "package.json", "payload-manifests.json"),
    ),
)
FORBIDDEN_RELEASE_SUFFIXES = (".pdb", ".map", ".rs", ".ts", ".tsx")
FORBIDDEN_RELEASE_DIR_NAMES = {".git", "node_modules", "target", "dist"}
SUPPORTED_WRAPPER_PLATFORMS = (
    ("win32", "x64"),
    ("darwin", "x64"),
    ("darwin", "arm64"),
    ("linux", "x64"),
    ("linux", "arm64"),
)

OS_MAP = {"darwin": "darwin", "linux": "linux", "win32": "win32"}
CPU_MAP = {
    "aarch64": "arm64",
    "amd64": "x64",
    "arm64": "arm64",
    "armv7l": "arm",
    "armv8l": "arm",
    "i386": "ia32",
    "i686": "ia32",
    "x86": "ia32",
    "x86_64": "x64",
}


@dataclass(frozen=True)
class PackageBuildResult:
    tarball_path: Path
    package_name: str
    version: str
    npm_os: str
    npm_cpu: str
    binary_path: Path | None
    kind: str = "platform"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build npm-installable tarballs for Rebon.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument(
        "--build",
        action="store_true",
        help="Run cargo build -p rebon-cli --release before packaging.",
    )
    parser.add_argument(
        "--binary",
        type=Path,
        help="Path to a prebuilt rebon binary. Defaults to target/release/rebon(.exe).",
    )
    parser.add_argument(
        "--package-name",
        help="npm package name for platform packages. Defaults to @rebon/cli-<os>-<cpu>.",
    )
    parser.add_argument(
        "--version",
        help="Override npm package version. Defaults to workspace.package.version.",
    )
    parser.add_argument(
        "--os",
        dest="npm_os",
        choices=["darwin", "linux", "win32"],
        help="Override npm os metadata.",
    )
    parser.add_argument(
        "--cpu",
        dest="npm_cpu",
        choices=["arm", "arm64", "ia32", "x64"],
        help="Override npm cpu metadata.",
    )
    parser.add_argument(
        "--out-dir",
        type=Path,
        default=DEFAULT_OUT_DIR,
        help="Directory that receives generated .tgz files.",
    )
    parser.add_argument(
        "--print-plugin-binaries",
        action="store_true",
        help="Print the derived plugin-binary manifest and exit without packaging.",
    )
    parser.add_argument(
        "--build-plugin-binaries",
        action="store_true",
        help="Build every binary in the derived plugin manifest and exit. "
        "The release jobs use this instead of naming the binaries themselves.",
    )
    parser.add_argument(
        "--target",
        help="Rust target triple for --build-plugin-binaries (the release cross-compiles).",
    )
    parser.add_argument(
        "--builder",
        default="cargo",
        help="Build command for --build-plugin-binaries (cargo, or cargo-zigbuild for musl).",
    )
    parser.add_argument(
        "--kind",
        choices=["platform", "wrapper", "bare", "all"],
        default="platform",
        help="Build the current platform package, root wrapper package (@rebon/cli), "
        "bare `rebon` alias package, or all of them.",
    )
    return parser.parse_args()


def read_workspace_field(field_name: str) -> str:
    text = CARGO_TOML.read_text(encoding="utf-8")
    section_match = re.search(r"(?ms)^\[workspace\.package\]\s*(.*?)(?:^\[|\Z)", text)
    if not section_match:
        raise SystemExit("Could not find [workspace.package] in Cargo.toml")
    field_match = re.search(
        rf'(?m)^{re.escape(field_name)}\s*=\s*"([^"]+)"\s*$', section_match.group(1)
    )
    if not field_match:
        raise SystemExit(f"Could not find workspace.package.{field_name} in Cargo.toml")
    return field_match.group(1)


def detect_npm_os() -> str:
    npm_os = OS_MAP.get(sys.platform)
    if npm_os is None:
        raise SystemExit(f"Unsupported platform for npm packaging: {sys.platform}")
    return npm_os


def detect_npm_cpu() -> str:
    machine = platform.machine().lower()
    npm_cpu = CPU_MAP.get(machine)
    if npm_cpu is None:
        raise SystemExit(f"Unsupported CPU for npm packaging: {machine}")
    return npm_cpu


def scoped_platform_package_name(npm_os: str, npm_cpu: str) -> str:
    return f"{PLATFORM_PACKAGE_SCOPE}/cli-{npm_os}-{npm_cpu}"


def default_package_name(npm_os: str, npm_cpu: str) -> str:
    return scoped_platform_package_name(npm_os, npm_cpu)


def default_binary_path(npm_os: str) -> Path:
    name = f"{DEFAULT_BIN_NAME}.exe" if npm_os == "win32" else DEFAULT_BIN_NAME
    return REPO_ROOT / "target" / "release" / name


def helper_binary_name(npm_os: str) -> str:
    return f"{BOA_HELPER_NAME}.exe" if npm_os == "win32" else BOA_HELPER_NAME


def sibling_helper_path(binary_path: Path, npm_os: str) -> Path:
    return ensure_binary(binary_path.with_name(helper_binary_name(npm_os)))


@dataclass(frozen=True)
class PluginBinary:
    """A `[[bin]]` a plugin crate ships beside `rebon`."""

    package: str
    name: str


def plugin_binaries() -> list[PluginBinary]:
    """Every `[[bin]]` declared by a crate under `crates/plugins/`.

    Derived rather than listed: adding a binary to a plugin has to be enough
    to get it packaged, or the first release after someone adds one ships a
    feature whose executable is missing.
    """
    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps", "--offline"],
            cwd=REPO_ROOT,
            check=True,
            capture_output=True,
            text=True,
            # `cargo metadata` emits UTF-8; the locale codec (GBK on a Chinese
            # Windows) fails on the first non-ASCII character in a package
            # description.
            encoding="utf-8",
        ).stdout
    )
    found: list[PluginBinary] = []
    for package in metadata["packages"]:
        manifest_dir = Path(package["manifest_path"]).resolve().parent
        if PLUGIN_CRATES_DIR not in manifest_dir.parents:
            continue
        for target in package["targets"]:
            if target["kind"] == ["bin"]:
                found.append(PluginBinary(package["name"], target["name"]))
    found.sort(key=lambda binary: binary.name)
    if not found:
        raise SystemExit(
            "cargo metadata reported no [[bin]] under crates/plugins; the plugin "
            "binary manifest cannot be empty."
        )
    return found


def build_plugin_binaries(
    binaries: list[PluginBinary],
    target: str | None = None,
    builder: str = "cargo",
) -> None:
    """Builds each plugin binary into the same `target/<triple>/release/` as `rebon`.

    `builder` and `target` exist because the release cross-compiles: the Linux
    packages are built with cargo-zigbuild from an Apple Silicon host, and a
    binary built for the host would land beside `rebon` and get packaged.
    """
    for binary in binaries:
        command = [
            builder,
            "build",
            "-p",
            binary.package,
            "--bin",
            binary.name,
            "--release",
            "--locked",
        ]
        if target:
            command.extend(["--target", target])
        subprocess.run(command, cwd=REPO_ROOT, check=True)


def plugin_binary_paths(binary_path: Path, npm_os: str) -> list[Path]:
    """The built plugin binaries beside `rebon`, failing on the first missing one."""
    paths = []
    for binary in plugin_binaries():
        name = f"{binary.name}.exe" if npm_os == "win32" else binary.name
        candidate = binary_path.with_name(name)
        if not candidate.exists():
            raise SystemExit(
                f"Plugin binary not found beside the rebon binary: {candidate}\n"
                f"Build it with `cargo build -p {binary.package} --bin {binary.name} --release`."
            )
        paths.append(candidate)
    return paths


def validate_plugin_binaries(package_dir: Path, bin_dir_name: str, npm_os: str) -> None:
    """Fail the build if a plugin's binary is not beside the packaged executable.

    Checked on the assembled package rather than trusted from the copy step,
    for the same reason as `validate_plane_scripts`: the failure that matters
    is a layout that moved.
    """
    bin_dir = package_dir / bin_dir_name
    missing = [
        binary.name
        for binary in plugin_binaries()
        if not (
            bin_dir / (f"{binary.name}.exe" if npm_os == "win32" else binary.name)
        ).is_file()
    ]
    if missing:
        formatted = "\n".join(f"  - {bin_dir_name}/{name}" for name in missing)
        raise SystemExit(
            "Refusing to package a release with a plugin's binary missing.\n"
            "Each one is a feature the product advertises, resolved beside the\n"
            "executable at runtime:\n"
            f"{formatted}"
        )


def sandbox_win_helper_path(binary_path: Path) -> Path | None:
    """The Windows sandbox helper built beside rebon.exe, if it is there.

    Optional rather than required. The sandbox is off by default, and a
    package built without the helper is a coherent thing: the caller reports
    "sandbox-win.exe was not found" with the step that fixes it, which is an
    accurate description of that package rather than a broken one.
    """
    candidate = binary_path.with_name(f"{SANDBOX_WIN_HELPER_NAME}.exe")
    return candidate if candidate.exists() else None


def ripgrep_platform_key(npm_os: str, npm_cpu: str) -> str:
    return f"{npm_cpu}-{npm_os}"


def ripgrep_binary_name(npm_os: str) -> str:
    return f"{RIPGREP_BINARY_NAME}.exe" if npm_os == "win32" else RIPGREP_BINARY_NAME


def vendor_ripgrep_path(npm_os: str, npm_cpu: str) -> Path:
    return (
        REPO_ROOT
        / "vendor"
        / "ripgrep"
        / ripgrep_platform_key(npm_os, npm_cpu)
        / ripgrep_binary_name(npm_os)
    )


def fetch_ripgrep_if_available(npm_os: str, npm_cpu: str) -> Path | None:
    rg_path = vendor_ripgrep_path(npm_os, npm_cpu)
    return rg_path if rg_path.exists() else None


def run_build() -> None:
    subprocess.run(
        ["cargo", "build", "-p", "rebon-cli", "--release"], cwd=REPO_ROOT, check=True
    )
    # Same pass as the main binary: the plugin binaries are required, and
    # building them separately is how a release ends up without one.
    build_plugin_binaries(plugin_binaries())
    # Built in the same pass, so a Windows package cannot end up without its
    # sandbox helper because someone ran one command and not the other.
    if platform.system() == "Windows" or os.environ.get("REBON_BUILD_SANDBOX_WIN"):
        subprocess.run(
            ["cargo", "build", "-p", "sandbox-win", "--release"],
            cwd=REPO_ROOT,
            check=True,
        )


def ensure_binary(binary_path: Path) -> Path:
    resolved = binary_path if binary_path.is_absolute() else REPO_ROOT / binary_path
    resolved = resolved.resolve()
    if not resolved.exists():
        raise SystemExit(f"Binary not found: {resolved}")
    if resolved.is_dir():
        raise SystemExit(f"Binary path is a directory: {resolved}")
    return resolved


def package_filename(
    package_name: str,
    version: str,
    npm_os: str | None = None,
    npm_cpu: str | None = None,
) -> str:
    safe_name = package_name.replace("@", "").replace("/", "-")
    suffix = f"-{npm_os}-{npm_cpu}" if npm_os and npm_cpu else ""
    return f"{safe_name}-{version}{suffix}.tgz"


def write_platform_package_json(
    package_dir: Path,
    package_name: str,
    version: str,
    license_expression: str,
    npm_os: str,
    npm_cpu: str,
    bin_name: str,
) -> None:
    bin_target = f"bin/{bin_name}"
    files = [
        "bin",
        "README.md",
        "LICENSE",
        "NOTICE",
        "THIRD_PARTY_NOTICES.txt",
        "scripts",
    ]
    scripts = {"postinstall": f"node scripts/{WINDOWS_POSTINSTALL_NAME}"}
    if npm_os == "win32":
        bin_target = f"bin/{WINDOWS_LAUNCHER_NAME}"
        files.extend(["payload", "lib"])
    package_json = {
        "name": package_name,
        "version": version,
        "description": DEFAULT_DESCRIPTION,
        "license": license_expression,
        "repository": {"type": "git", "url": REPOSITORY_URL},
        "homepage": HOMEPAGE_URL,
        "engines": {"node": ">=20"},
        "bin": {DEFAULT_BIN_NAME: bin_target},
        "files": files,
        "os": [npm_os],
        "cpu": [npm_cpu],
    }
    package_json["scripts"] = scripts
    (package_dir / "package.json").write_text(
        json.dumps(package_json, indent=2) + "\n", encoding="utf-8"
    )


def write_wrapper_package_json(
    package_dir: Path, version: str, license_expression: str
) -> None:
    optional_dependencies = {
        scoped_platform_package_name(os, cpu): version
        for os, cpu in SUPPORTED_WRAPPER_PLATFORMS
    }
    package_json = {
        "name": WRAPPER_PACKAGE_NAME,
        "version": version,
        "description": DEFAULT_DESCRIPTION,
        "license": license_expression,
        "repository": {"type": "git", "url": REPOSITORY_URL},
        "homepage": HOMEPAGE_URL,
        "engines": {"node": ">=20"},
        "bin": {DEFAULT_BIN_NAME: "bin/rebon.js"},
        "files": ["bin", "README.md", "LICENSE", "NOTICE"],
        "optionalDependencies": optional_dependencies,
    }
    (package_dir / "package.json").write_text(
        json.dumps(package_json, indent=2) + "\n", encoding="utf-8"
    )


def copy_readme(package_dir: Path) -> None:
    if README_MD.exists():
        shutil.copy2(README_MD, package_dir / "README.md")


def copy_third_party_notices(package_dir: Path) -> None:
    shutil.copy2(THIRD_PARTY_NOTICES, package_dir / THIRD_PARTY_NOTICES.name)


# Apache-2.0 section 4 requires every redistribution to carry both the licence
# and the NOTICE text, so the two travel in the tarball rather than only in the
# repository.
def copy_license(package_dir: Path) -> None:
    shutil.copy2(LICENSE_FILE, package_dir / "LICENSE")
    shutil.copy2(NOTICE_FILE, package_dir / "NOTICE")


def copy_binary(
    package_dir: Path,
    binary_path: Path,
    bin_name: str,
    npm_os: str,
    helper_path: Path,
    ripgrep_path: Path | None = None,
    plugin_binary_files: list[Path] | None = None,
) -> None:
    bin_dir = package_dir / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    target = bin_dir / bin_name
    shutil.copy2(binary_path, target)
    helper_target = bin_dir / helper_binary_name(npm_os)
    shutil.copy2(helper_path, helper_target)
    if npm_os != "win32":
        executable_mode = stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH
        target.chmod(target.stat().st_mode | executable_mode)
        helper_target.chmod(helper_target.stat().st_mode | executable_mode)
    for plugin_binary in plugin_binary_files or []:
        plugin_target = bin_dir / plugin_binary.name
        shutil.copy2(plugin_binary, plugin_target)
        if npm_os != "win32":
            plugin_target.chmod(
                plugin_target.stat().st_mode
                | stat.S_IXUSR
                | stat.S_IXGRP
                | stat.S_IXOTH
            )
    if ripgrep_path is not None and ripgrep_path.exists():
        rg_target = bin_dir / ripgrep_binary_name(npm_os)
        shutil.copy2(ripgrep_path, rg_target)
        if npm_os != "win32":
            rg_target.chmod(
                rg_target.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH
            )
    copy_plane_scripts(bin_dir)


def copy_plane_scripts(dest_dir: Path) -> None:
    for package_name, entries in PLANE_SCRIPT_SETS:
        source_root = NODE_ROOT / package_name
        target_root = dest_dir / package_name
        for entry in entries:
            source = source_root / entry
            if not source.exists():
                raise SystemExit(f"Plugin plane asset missing: {source}")
            target = target_root / entry
            if source.is_dir():
                shutil.copytree(source, target, dirs_exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, target)


def copy_postinstall_script(package_dir: Path) -> None:
    scripts_dir = package_dir / "scripts"
    scripts_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(
        NPM_ASSETS_ROOT / "scripts" / WINDOWS_POSTINSTALL_NAME,
        scripts_dir / WINDOWS_POSTINSTALL_NAME,
    )


def copy_windows_assets(
    package_dir: Path,
    binary_path: Path,
    helper_path: Path,
    ripgrep_path: Path | None = None,
    sandbox_win_path: Path | None = None,
    plugin_binary_files: list[Path] | None = None,
) -> None:
    bin_dir = package_dir / "bin"
    payload_dir = package_dir / "payload"
    scripts_dir = package_dir / "scripts"
    lib_dir = package_dir / "lib"
    bin_dir.mkdir(parents=True, exist_ok=True)
    payload_dir.mkdir(parents=True, exist_ok=True)
    scripts_dir.mkdir(parents=True, exist_ok=True)
    lib_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(
        NPM_ASSETS_ROOT / "bin" / WINDOWS_LAUNCHER_NAME, bin_dir / WINDOWS_LAUNCHER_NAME
    )
    shutil.copy2(binary_path, payload_dir / WINDOWS_PAYLOAD_NAME)
    shutil.copy2(helper_path, payload_dir / helper_binary_name("win32"))
    # Into payload/, beside rebon.exe: `sibling_binary::candidates` checks the
    # running executable's own directory first, so this is the only place a
    # plugin's binary can be and still be found.
    for plugin_binary in plugin_binary_files or []:
        shutil.copy2(plugin_binary, payload_dir / plugin_binary.name)
    # Beside the exe: the managed install copies payload/ wholesale, so the
    # plane's scripts follow rebon.exe wherever it ends up running from.
    copy_plane_scripts(payload_dir)
    if ripgrep_path is not None and ripgrep_path.exists():
        shutil.copy2(ripgrep_path, payload_dir / ripgrep_binary_name("win32"))
    if sandbox_win_path is not None and sandbox_win_path.exists():
        # Into payload/, beside rebon.exe. `sandbox_win_candidates` checks the
        # running executable's directory ahead of %ProgramFiles%, so an npm
        # install picks up this helper rather than some machine-wide copy on a
        # different contract version.
        shutil.copy2(sandbox_win_path, payload_dir / f"{SANDBOX_WIN_HELPER_NAME}.exe")
    shutil.copy2(
        NPM_ASSETS_ROOT / "scripts" / WINDOWS_POSTINSTALL_NAME,
        scripts_dir / WINDOWS_POSTINSTALL_NAME,
    )
    shutil.copy2(
        NPM_ASSETS_ROOT / "lib" / WINDOWS_MANAGED_LIB_NAME,
        lib_dir / WINDOWS_MANAGED_LIB_NAME,
    )


def copy_wrapper_assets(package_dir: Path) -> None:
    bin_dir = package_dir / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    target = bin_dir / "rebon.js"
    shutil.copy2(WRAPPER_LAUNCHER, target)
    target.chmod(target.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def validate_release_payload(package_dir: Path) -> None:
    """Reject accidental debug/source/build artifacts before tarball creation."""
    blocked: list[str] = []
    for path in package_dir.rglob("*"):
        rel = path.relative_to(package_dir).as_posix()
        if any(
            part in FORBIDDEN_RELEASE_DIR_NAMES
            for part in path.relative_to(package_dir).parts
        ):
            blocked.append(rel)
        elif path.is_dir() and path.name.endswith(".dSYM"):
            blocked.append(rel)
        elif path.is_file() and path.suffix.lower() in FORBIDDEN_RELEASE_SUFFIXES:
            blocked.append(rel)
    if blocked:
        formatted = "\n".join(f"  - {item}" for item in sorted(blocked))
        raise SystemExit(
            f"Refusing to package debug/source/build artifacts:\n{formatted}"
        )


# What `locate_host_script` and `locate_compose_loader` look for beside the
# executable. A platform package that ships without them boots with the
# plugin plane down and says so only at WARN — which is how a whole
# distribution can run for a release without anyone noticing
# (harness-defects-tb40 §5). Checked on the assembled package rather than
# trusted from `copy_plane_scripts`, because the failure that matters is a
# layout that moved, not a source file that vanished.
PLANE_ENTRYPOINTS = (
    "plugin-host/src/cli.mjs",
    "compose-runtime/src/index.mjs",
)


def plane_bin_dir_name(npm_os: str) -> str:
    """Where the executable lands, and therefore where the plane must land.

    Windows ships the exe under `payload/` because the managed install
    copies that directory wholesale; every other platform puts it in `bin/`.
    """
    return "payload" if npm_os == "win32" else "bin"


def validate_plane_scripts(package_dir: Path, bin_dir_name: str) -> None:
    """Fail the build if the plugin plane's entrypoints are not beside the binary."""
    bin_dir = package_dir / bin_dir_name
    missing = [
        relative
        for relative in PLANE_ENTRYPOINTS
        if not (bin_dir / relative).is_file()
    ]
    if missing:
        formatted = "\n".join(f"  - {bin_dir_name}/{item}" for item in missing)
        raise SystemExit(
            "Refusing to package a binary with no plugin plane beside it.\n"
            "These are resolved relative to the executable at runtime, and a\n"
            "missing one degrades to a WARN nobody reads:\n"
            f"{formatted}"
        )


def build_tarball(package_dir: Path, tarball_path: Path) -> None:
    validate_release_payload(package_dir)
    tarball_path.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(tarball_path, "w:gz") as tar:
        tar.add(package_dir, arcname="package")


def build_platform_package(
    *,
    build: bool,
    binary: Path | None,
    package_name: str | None,
    version: str | None,
    npm_os: str | None,
    npm_cpu: str | None,
    out_dir: Path,
) -> PackageBuildResult:
    resolved_npm_os = npm_os or detect_npm_os()
    resolved_npm_cpu = npm_cpu or detect_npm_cpu()
    resolved_package_name = package_name or default_package_name(
        resolved_npm_os, resolved_npm_cpu
    )
    resolved_version = version or read_workspace_field("version")
    license_expression = read_workspace_field("license")
    if build:
        run_build()
    binary_path = ensure_binary(binary or default_binary_path(resolved_npm_os))
    helper_path = sibling_helper_path(binary_path, resolved_npm_os)
    plugin_binary_files = plugin_binary_paths(binary_path, resolved_npm_os)
    ripgrep_path = fetch_ripgrep_if_available(resolved_npm_os, resolved_npm_cpu)
    sandbox_win_path = (
        sandbox_win_helper_path(binary_path) if resolved_npm_os == "win32" else None
    )
    if resolved_npm_os == "win32" and sandbox_win_path is None:
        print(
            "Warning: sandbox-win.exe was not found beside the binary; the Windows "
            "package will ship without the sandbox helper, and `sandbox.enabled` will "
            "report it as missing. Build it with `cargo build -p sandbox-win --release`.",
            file=sys.stderr,
        )
    if ripgrep_path is None:
        print(
            f"Warning: ripgrep {RIPGREP_VERSION} for {ripgrep_platform_key(resolved_npm_os, resolved_npm_cpu)} "
            "was not found/fetched; package will rely on PATH rg or native fallback.",
            file=sys.stderr,
        )
    bin_name = (
        f"{DEFAULT_BIN_NAME}.exe" if resolved_npm_os == "win32" else DEFAULT_BIN_NAME
    )
    tarball_path = out_dir / package_filename(
        resolved_package_name, resolved_version, resolved_npm_os, resolved_npm_cpu
    )
    with tempfile.TemporaryDirectory(prefix="rebon-npm-") as tmp:
        package_dir = Path(tmp) / "package"
        package_dir.mkdir(parents=True, exist_ok=True)
        write_platform_package_json(
            package_dir,
            resolved_package_name,
            resolved_version,
            license_expression,
            resolved_npm_os,
            resolved_npm_cpu,
            bin_name,
        )
        copy_readme(package_dir)
        copy_license(package_dir)
        copy_third_party_notices(package_dir)
        if resolved_npm_os == "win32":
            copy_windows_assets(
                package_dir,
                binary_path,
                helper_path,
                ripgrep_path,
                sandbox_win_path,
                plugin_binary_files,
            )
        else:
            copy_binary(
                package_dir,
                binary_path,
                bin_name,
                resolved_npm_os,
                helper_path,
                ripgrep_path,
                plugin_binary_files,
            )
            copy_postinstall_script(package_dir)
        validate_plane_scripts(package_dir, plane_bin_dir_name(resolved_npm_os))
        validate_plugin_binaries(
            package_dir, plane_bin_dir_name(resolved_npm_os), resolved_npm_os
        )
        build_tarball(package_dir, tarball_path)
    return PackageBuildResult(
        tarball_path,
        resolved_package_name,
        resolved_version,
        resolved_npm_os,
        resolved_npm_cpu,
        binary_path,
        "platform",
    )


def build_wrapper_package(*, version: str | None, out_dir: Path) -> PackageBuildResult:
    resolved_version = version or read_workspace_field("version")
    license_expression = read_workspace_field("license")
    tarball_path = out_dir / package_filename(WRAPPER_PACKAGE_NAME, resolved_version)
    with tempfile.TemporaryDirectory(prefix="rebon-npm-wrapper-") as tmp:
        package_dir = Path(tmp) / "package"
        package_dir.mkdir(parents=True, exist_ok=True)
        write_wrapper_package_json(package_dir, resolved_version, license_expression)
        copy_readme(package_dir)
        copy_license(package_dir)
        copy_wrapper_assets(package_dir)
        build_tarball(package_dir, tarball_path)
    return PackageBuildResult(
        tarball_path,
        WRAPPER_PACKAGE_NAME,
        resolved_version,
        "any",
        "any",
        None,
        "wrapper",
    )


def build_bare_package(*, version: str | None, out_dir: Path) -> PackageBuildResult:
    resolved_version = version or read_workspace_field("version")
    template_path = BARE_ASSETS_ROOT / "package.json"
    package_json = json.loads(template_path.read_text(encoding="utf-8"))
    if package_json.get("name") != BARE_PACKAGE_NAME:
        raise SystemExit(
            f"{template_path} declares name {package_json.get('name')!r}; "
            f"expected {BARE_PACKAGE_NAME!r}"
        )
    if WRAPPER_PACKAGE_NAME not in package_json.get("dependencies", {}):
        raise SystemExit(
            f"{template_path} must depend on {WRAPPER_PACKAGE_NAME}"
        )
    # The checked-in file is a template: version and the @rebon/cli range are
    # derived from workspace.package.version so the bare alias can never lag
    # behind a release again (a stale ^0.x range pins users to the old minor).
    package_json["version"] = resolved_version
    package_json["dependencies"][WRAPPER_PACKAGE_NAME] = f"^{resolved_version}"
    tarball_path = out_dir / package_filename(BARE_PACKAGE_NAME, resolved_version)
    with tempfile.TemporaryDirectory(prefix="rebon-npm-bare-") as tmp:
        package_dir = Path(tmp) / "package"
        package_dir.mkdir(parents=True, exist_ok=True)
        (package_dir / "package.json").write_text(
            json.dumps(package_json, indent=2) + "\n", encoding="utf-8"
        )
        shutil.copy2(BARE_ASSETS_ROOT / "README.md", package_dir / "README.md")
        copy_license(package_dir)
        bin_dir = package_dir / "bin"
        bin_dir.mkdir(parents=True, exist_ok=True)
        launcher = bin_dir / "rebon.js"
        shutil.copy2(BARE_ASSETS_ROOT / "bin" / "rebon.js", launcher)
        launcher.chmod(
            launcher.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH
        )
        build_tarball(package_dir, tarball_path)
    return PackageBuildResult(
        tarball_path,
        BARE_PACKAGE_NAME,
        resolved_version,
        "any",
        "any",
        None,
        "bare",
    )


def build_package(**kwargs) -> PackageBuildResult:
    return build_platform_package(**kwargs)


def build_packages(
    *,
    kind: str,
    build: bool,
    binary: Path | None,
    package_name: str | None,
    version: str | None,
    npm_os: str | None,
    npm_cpu: str | None,
    out_dir: Path,
) -> list[PackageBuildResult]:
    results: list[PackageBuildResult] = []
    if kind in ("platform", "all"):
        results.append(
            build_platform_package(
                build=build,
                binary=binary,
                package_name=package_name,
                version=version,
                npm_os=npm_os,
                npm_cpu=npm_cpu,
                out_dir=out_dir,
            )
        )
    elif build:
        run_build()
    if kind in ("wrapper", "all"):
        results.append(build_wrapper_package(version=version, out_dir=out_dir))
    if kind in ("bare", "all"):
        results.append(build_bare_package(version=version, out_dir=out_dir))
    return results


def print_summary(result: PackageBuildResult) -> None:
    print(f"Built {result.tarball_path}")
    print(f"Package: {result.package_name}@{result.version}")
    print(f"Kind: {result.kind}")
    print(f"Platform: {result.npm_os}/{result.npm_cpu}")
    if result.binary_path is not None:
        print(f"Binary: {result.binary_path}")
    print(f"Install local:  npm install {result.tarball_path}")
    print(f"Install global: npm install -g {result.tarball_path}")


def main() -> None:
    args = parse_args()
    if args.print_plugin_binaries:
        for binary in plugin_binaries():
            print(f"{binary.package}\t{binary.name}")
        return
    if args.build_plugin_binaries:
        build_plugin_binaries(plugin_binaries(), args.target, args.builder)
        return
    for result in build_packages(
        kind=args.kind,
        build=args.build,
        binary=args.binary,
        package_name=args.package_name,
        version=args.version,
        npm_os=args.npm_os,
        npm_cpu=args.npm_cpu,
        out_dir=args.out_dir,
    ):
        print_summary(result)


if __name__ == "__main__":
    main()
