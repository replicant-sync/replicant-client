#!/usr/bin/env python3
"""
Build distribution headers and libraries.
Creates a stable SDK in the dist/ folder.
"""

import argparse
import json
import os
import platform
import shutil
import subprocess
import sys
from datetime import datetime
from pathlib import Path


def run_command(cmd: list[str], check: bool = True) -> subprocess.CompletedProcess:
    """Run a command and return the result."""
    return subprocess.run(cmd, check=check, capture_output=True, text=True)


def main():
    parser = argparse.ArgumentParser(description="Build distribution SDK")
    parser.add_argument("--skip-build", action="store_true",
                        help="Skip cargo build (use pre-built libraries)")
    args = parser.parse_args()

    # Ensure we're in the workspace root
    script_dir = Path(__file__).parent
    workspace_root = script_dir.parent
    os.chdir(workspace_root)

    if args.skip_build:
        print("Skipping build (using pre-built libraries)...")
    else:
        print("Building replicant-client library...")
        result = run_command(["cargo", "build", "--package", "replicant-client", "--release"])
        if result.returncode != 0:
            print(f"Build failed: {result.stderr}")
            sys.exit(1)

    print("Creating dist directory structure...")
    dist = Path("dist")
    for subdir in ["include", "lib", "cmake"]:
        (dist / subdir).mkdir(parents=True, exist_ok=True)

    # Copy C header
    print("Copying generated C header to dist...")
    c_header = Path("replicant-client/target/include/replicant.h")
    if c_header.exists():
        shutil.copy(c_header, dist / "include" / "replicant.h")
        print("[OK] C header copied to dist/include/replicant.h")
    else:
        print("[FAIL] Generated C header not found. Make sure cargo build completed successfully.")
        sys.exit(1)

    # Copy C++ wrapper header
    print("Copying C++ wrapper header to dist...")
    cpp_header = Path("replicant-client/include/replicant.hpp")
    if cpp_header.exists():
        shutil.copy(cpp_header, dist / "include" / "replicant.hpp")
        print("[OK] C++ header copied to dist/include/replicant.hpp")
    else:
        print("[WARN] C++ header not found at replicant-client/include/replicant.hpp")

    # Copy libraries (platform-specific)
    print("Copying built libraries to dist...")
    system = platform.system()

    if system == "Darwin":  # macOS
        libs = [
            ("target/release/libreplicant_client.a", "Static library"),
            ("target/release/libreplicant_client.dylib", "Dynamic library"),
        ]
    elif system == "Linux":
        libs = [
            ("target/release/libreplicant_client.a", "Static library"),
            ("target/release/libreplicant_client.so", "Shared library"),
        ]
    elif system == "Windows":
        libs = [
            ("target/release/replicant_client.lib", "Static library"),
            ("target/release/replicant_client.dll", "Dynamic library"),
        ]
    else:
        libs = []
        print(f"[WARN] Unknown platform: {system}")

    for lib_path, lib_type in libs:
        lib = Path(lib_path)
        if lib.exists():
            shutil.copy(lib, dist / "lib" / lib.name)
            print(f"[OK] {lib_type} copied to dist/lib/")

    # Get version from Cargo
    print("Adding version information...")
    result = run_command(["cargo", "metadata", "--no-deps", "--format-version", "1"], check=False)
    version = "unknown"
    if result.returncode == 0:
        metadata = json.loads(result.stdout)
        for pkg in metadata.get("packages", []):
            if pkg.get("name") == "replicant-client":
                version = pkg.get("version", "unknown")
                break

    # Get Rust version
    rust_result = run_command(["rustc", "--version"], check=False)
    rust_version = rust_result.stdout.strip() if rust_result.returncode == 0 else "unknown"

    # Write version file
    version_file = dist / "VERSION.md"
    version_file.write_text(f"""# Replicant SDK v{version}

Generated on: {datetime.now().isoformat()}
Rust version: {rust_version}
Platform: {platform.system()} {platform.machine()}
""")

    print()
    print("[OK] Distribution build complete!")
    print("SDK available in dist/ folder:")
    print("  - dist/include/replicant.h     (C header - auto-generated)")
    print("  - dist/include/replicant.hpp   (C++ wrapper)")
    print("  - dist/lib/                    (compiled libraries)")
    print()
    print(f"Version: {version}")


if __name__ == "__main__":
    main()
