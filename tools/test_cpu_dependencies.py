import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from check_cpu_dependencies import validate_cpu_tree

ROOT = "alexandria v0.2.0 (/checkout)\n"


class CpuDependenciesTests(unittest.TestCase):
    def test_allows_cpu_burn_and_shared_cubecl_utilities(self):
        validate_cpu_tree(ROOT + "burn-flex v0.22.0\ncubecl-common v0.11.0\n"
                          "cubecl-environment v0.11.0 (*)\nburn-derive v0.22.0 (proc-macro)\n")

    def test_rejects_gpu_backends_and_native_transitive_bindings(self):
        for name in ("burn-rocm", "burn-wgpu", "burn-cuda", "burn-hip",
                     "cubecl-hip", "cubecl-hip-sys", "cubecl-wgpu", "cubecl-cuda",
                     "cubecl-cpp", "cubecl-spirv", "wgpu", "wgpu-core", "wgpu-hal",
                     "wgpu-types", "ash", "metal", "cudarc", "hip-runtime-sys",
                     "cuda-runtime-sys"):
            with self.subTest(name=name):
                with self.assertRaisesRegex(ValueError, name):
                    validate_cpu_tree(ROOT + f"{name} v1.2.3 (*)\n")

    def test_rejects_missing_root_and_malformed_graphs(self):
        for graph in ("", "burn-flex v0.22.0\n", ROOT + "not a package\n",
                      "alexandria vbroken\n", ROOT + "warning: failed to resolve\n"):
            with self.subTest(graph=graph):
                with self.assertRaises(ValueError):
                    validate_cpu_tree(graph)


class CpuDependenciesCliTests(unittest.TestCase):
    def run_guard(self, graph, cargo_exit=0):
        with tempfile.TemporaryDirectory() as directory:
            cargo = Path(directory) / "cargo"
            expected = ["tree", "--locked", "-p", "alexandria", "--no-default-features",
                        "--edges", "normal,build", "--prefix", "none", "--format", "{p}"]
            cargo.write_text("#!/usr/bin/env python3\nimport sys\n"
                             f"assert sys.argv[1:] == {expected!r}\n"
                             f"print({graph!r}, end='')\n"
                             f"sys.exit({cargo_exit})\n")
            cargo.chmod(0o700)
            env = {**os.environ, "PATH": directory + os.pathsep + os.environ.get("PATH", "")}
            return subprocess.run([sys.executable, "-B", str(Path(__file__).with_name(
                "check_cpu_dependencies.py"))], env=env, text=True, capture_output=True,
                timeout=10, check=False)

    def test_cpu_graph_succeeds_but_gpu_graph_fails(self):
        result = self.run_guard(ROOT + "burn-flex v0.22.0\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.run_guard(ROOT + "cubecl-hip-sys v7.14.0\n")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("cubecl-hip-sys", result.stderr)

    def test_cargo_failure_cannot_accept_partial_cpu_graph(self):
        result = self.run_guard(ROOT, cargo_exit=23)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("cargo tree failed", result.stderr)

    def test_missing_cargo_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run([sys.executable, "-B", str(Path(__file__).with_name(
                "check_cpu_dependencies.py"))], env={**os.environ, "PATH": directory},
                text=True, capture_output=True, timeout=10, check=False)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("cargo tree failed", result.stderr)


if __name__ == "__main__":
    unittest.main()
