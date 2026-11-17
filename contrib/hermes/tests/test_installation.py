"""Exercise the CI installer against controlled local artifacts, without networking."""
import hashlib
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import unittest
import venv
import zipfile


class InstallationPolicy(unittest.TestCase):
    def test_ci_refuses_a_wheel_that_no_longer_matches_the_lock(self):
        root = Path(__file__).resolve().parents[3]
        workflow = (root / ".github/workflows/ci.yml").read_text()
        job = re.search(
            r"(?ms)^  hermes-companion:\n(.*?)(?=^  \S|\Z)", workflow
        ).group(1)
        command = shlex.split(next(
            line.strip().removeprefix("run: ")
            for line in job.splitlines()
            if line.strip().startswith("run: python -m pip install ")
        ))

        with tempfile.TemporaryDirectory() as directory:
            sandbox = Path(directory)
            environment = sandbox / "venv"
            venv.EnvBuilder(with_pip=True).create(environment)
            command[0] = str(environment / ("Scripts/python.exe" if os.name == "nt" else "bin/python"))
            wheel = sandbox / "httpx-0.28.1-py3-none-any.whl"
            with zipfile.ZipFile(wheel, "w") as archive:
                archive.writestr("httpx/__init__.py", "")
                archive.writestr(
                    "httpx-0.28.1.dist-info/METADATA",
                    "Metadata-Version: 2.1\nName: httpx\nVersion: 0.28.1\n",
                )
                archive.writestr(
                    "httpx-0.28.1.dist-info/WHEEL",
                    "Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
                )
                archive.writestr("httpx-0.28.1.dist-info/RECORD", "")
            lock = sandbox / "contrib/hermes/requirements-test.txt"
            lock.parent.mkdir(parents=True)
            env = {key: val for key, val in os.environ.items() if not key.startswith("PIP_")}
            env.update({
                "PIP_CONFIG_FILE": os.devnull,
                "PIP_NO_INDEX": "1",
                "PIP_FIND_LINKS": str(sandbox),
                "PIP_DISABLE_PIP_VERSION_CHECK": "1",
                "PIP_IGNORE_INSTALLED": "1",
                "VIRTUAL_ENV": str(environment),
            })
            correct_hash = hashlib.sha256(wheel.read_bytes()).hexdigest()
            for digest, accepted in [(correct_hash, True), ("0" * 64, False)]:
                with self.subTest(accepted=accepted):
                    lock.write_text(f"httpx==0.28.1 --hash=sha256:{digest}\n")
                    result = subprocess.run(
                        command, cwd=sandbox, env=env, text=True,
                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=60,
                    )
                    if accepted:
                        self.assertEqual(result.returncode, 0, result.stdout)
                    else:
                        self.assertNotEqual(result.returncode, 0, result.stdout)
                        self.assertIn("hashes", result.stdout.lower())
