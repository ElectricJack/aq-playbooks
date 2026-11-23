"""Build and coordinate the frozen PostgreSQL writers used by the compatibility probes."""

import argparse
from contextlib import contextmanager
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import tarfile
import tempfile
from urllib.parse import urlparse


def checked_archive(expected_hash, database_prefix):
    parser = argparse.ArgumentParser()
    parser.add_argument("--old-source-archive", type=Path, required=True)
    archive = parser.parse_args().old_source_archive
    actual_hash = hashlib.sha256(archive.read_bytes()).hexdigest()
    assert actual_hash == expected_hash
    url = urlparse(os.environ["AQ_PLAYBOOKS_TEST_DATABASE_URL"])
    assert (
        url.hostname == "127.0.0.1"
        and url.port == 55432
        and url.path.startswith(database_prefix)
    )
    return archive, actual_hash


@dataclass(frozen=True)
class FrozenWriter:
    name: str
    version: str
    program: str

    def build(self, archive, base):
        old = base / "old"
        old.mkdir()
        with tarfile.open(archive) as source:
            source.extractall(old, filter="data")
        probe = base / "probe"
        (probe / "src").mkdir(parents=True)
        packages = ["aq-playbooks-postgres", "aq-playbooks-runner"]
        dependencies = "".join(
            f'{name}={{path={json.dumps(str(old / "crates" / name))},version="={self.version}"}}\n'
            for name in packages
        )
        manifest = f'[package]\nname="{self.name}"\nversion="0.0.0"\nedition="2024"\n[dependencies]\n'
        (probe / "Cargo.toml").write_text(manifest + dependencies)
        (probe / "src/main.rs").write_text(self.program)
        subprocess.run(
            [
                "cargo",
                "build",
                "--offline",
                "--manifest-path",
                str(probe / "Cargo.toml"),
            ],
            check=True,
        )
        return probe

    @contextmanager
    def start(self, archive, prefix):
        os.environ["CARGO_INCREMENTAL"] = "0"
        with tempfile.TemporaryDirectory(prefix=prefix) as temp:
            probe = self.build(archive, Path(temp))
            target = Path(os.environ.get("CARGO_TARGET_DIR", str(probe / "target")))
            child = subprocess.Popen(
                [str(target / "debug" / self.name)],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                text=True,
            )
            try:
                yield OldOwner(child, probe / "Cargo.lock")
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()


@dataclass(frozen=True)
class OldOwner:
    process: subprocess.Popen
    lockfile: Path

    def wait_ready(self, expected, timeout_message):
        assert select.select([self.process.stdout], [], [], 30)[0], timeout_message
        assert self.process.stdout.readline().strip() == expected

    def expect_refusals(self, expected):
        stdout, _ = self.process.communicate("resume\n", timeout=30)
        assert self.process.returncode == 0 and stdout.splitlines() == expected, stdout


def run_example(name, *arguments):
    command = [
        "cargo",
        "run",
        "--locked",
        "--offline",
        "-q",
        "-p",
        "aq-playbooks-postgres",
        "--example",
        name,
        "--",
    ]
    root = Path(__file__).resolve().parents[1]
    result = subprocess.run(
        command + list(arguments),
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
        timeout=120,
    )
    return json.loads(result.stdout)
