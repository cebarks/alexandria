"""Guard the CPU binary's normal/build graph against known GPU dependencies."""

import re
import subprocess
import sys

GPU_FAMILIES = (
    "burn-rocm", "burn-wgpu", "burn-cuda", "burn-hip",
    "cubecl-hip", "cubecl-wgpu", "cubecl-cuda", "cubecl-cpp", "cubecl-spirv",
    "wgpu", "ash", "metal", "cudarc", "hip-runtime-sys", "cuda-runtime-sys",
)


def validate_cpu_tree(text: str) -> None:
    packages = set()
    for line in text.splitlines():
        if not line.strip():
            continue
        match = re.fullmatch(r"([A-Za-z0-9_-]+) v\d+\.\d+\.\d+[^\s]*(?: .*)?", line)
        if match is None:
            raise ValueError(f"Malformed dependency graph line: {line!r}")
        packages.add(match.group(1))
    if "alexandria" not in packages:
        raise ValueError("CPU dependency graph has no alexandria root")
    forbidden = sorted(name for name in packages if any(
        name == family or name.startswith(family + "-") for family in GPU_FAMILIES))
    if forbidden:
        raise ValueError("CPU graph includes GPU dependencies: " + ", ".join(forbidden))


def main() -> int:
    try:
        tree = subprocess.run(
            ["cargo", "tree", "--locked", "-p", "alexandria", "--no-default-features",
             "--edges", "normal,build", "--prefix", "none", "--format", "{p}"],
            check=True, capture_output=True, text=True, timeout=120,
        )
    except (OSError, subprocess.SubprocessError) as error:
        print(f"cargo tree failed: {error}", file=sys.stderr)
        return 1
    try:
        validate_cpu_tree(tree.stdout)
    except ValueError as error:
        print(error, file=sys.stderr)
        return 1
    print("CPU graph has no known GPU backend/native runtime dependencies")
    return 0


if __name__ == "__main__":
    sys.exit(main())
