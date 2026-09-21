"""Build and publish Rebon npm tarballs.

Usage:
    python scripts/publish_npm_package.py --package-name @rebon/cli-win32-x64 --dry-run
    python scripts/publish_npm_package.py --kind all --tag latest
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

from build_npm_package import DEFAULT_OUT_DIR, REPO_ROOT, build_packages, print_summary

DEFAULT_TAG = "latest"
SCOPED_PUBLIC_PREFIX = "@rebon/"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build and publish Rebon npm tarballs.",
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
        help="npm package name for platform package publishes. Defaults to @rebon/cli-<os>-<cpu>.",
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
        "--kind",
        choices=["platform", "wrapper", "all"],
        default="platform",
        help="Publish the current platform package, root wrapper package, or platform first then wrapper.",
    )
    parser.add_argument(
        "--tag", default=DEFAULT_TAG, help="npm dist-tag to publish under."
    )
    parser.add_argument(
        "--access",
        choices=["public", "restricted"],
        help="Pass through npm publish access mode. Defaults to public for @rebon scoped packages.",
    )
    parser.add_argument(
        "--dry-run", action="store_true", help="Run npm publish in dry-run mode."
    )
    parser.add_argument("--otp", help="One-time password for npm 2FA.")
    return parser.parse_args()


def npm_executable() -> str:
    return "npm.cmd" if sys.platform == "win32" else "npm"


def access_for_package(args: argparse.Namespace, package_name: str) -> str | None:
    if args.access:
        return args.access
    if package_name.startswith(SCOPED_PUBLIC_PREFIX):
        return "public"
    return None


def build_publish_command(
    args: argparse.Namespace, tarball_path: Path, package_name: str
) -> list[str]:
    command = [npm_executable(), "publish", str(tarball_path), "--tag", args.tag]
    access = access_for_package(args, package_name)
    if access:
        command.extend(["--access", access])
    if args.dry_run:
        command.append("--dry-run")
    if args.otp:
        command.extend(["--otp", args.otp])
    return command


def main() -> None:
    args = parse_args()
    results = build_packages(
        kind=args.kind,
        build=args.build,
        binary=args.binary,
        package_name=args.package_name,
        version=args.version,
        npm_os=args.npm_os,
        npm_cpu=args.npm_cpu,
        out_dir=args.out_dir,
    )

    for result in results:
        print_summary(result)

    for result in results:
        command = build_publish_command(args, result.tarball_path, result.package_name)
        print("Publish command:", " ".join(command))
        subprocess.run(command, cwd=REPO_ROOT, check=True)


if __name__ == "__main__":
    main()
