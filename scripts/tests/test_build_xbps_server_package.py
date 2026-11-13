import importlib.util
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/ci/build-xbps-server-package.py"
SPEC = importlib.util.spec_from_file_location("build_xbps_server_package", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"Unable to load XBPS package helper: {SCRIPT}")
XBPS_BUILDER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(XBPS_BUILDER)


class XbpsServerPackageTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.output = self.root / "output"
        self.stage = self.root / "stage"
        self.repository = self.root / "repository"
        self.stage.mkdir()
        self.output.mkdir()
        (self.output / "server.tar.gz").write_bytes(b"tar")
        (self.output / "server.tar.zst").write_bytes(b"tar-zstd")

        self.tools = self.root / "tools"
        self.tools.mkdir()
        for name in ("xbps-create", "xbps-rindex", "xbps-query"):
            (self.tools / name).write_text("#!/bin/sh\n", encoding="utf-8")

        self.install_hook = self.root / "INSTALL"
        self.remove_hook = self.root / "REMOVE"
        self.install_hook.write_text("sv restart gitrun\n", encoding="utf-8")
        self.remove_hook.write_text("sv down gitrun\n", encoding="utf-8")

        self.args = SimpleNamespace(
            output=self.output,
            stage=self.stage,
            repository=self.repository,
            create=str(self.tools / "xbps-create"),
            rindex=str(self.tools / "xbps-rindex"),
            query=str(self.tools / "xbps-query"),
            architecture="x86_64",
            dependency="glibc",
            package_id="gitrun-server-1.9.1_1",
            version="v1.9.1",
            cpu="x86_64",
            install_hook=self.install_hook,
            remove_hook=self.remove_hook,
        )

    def test_keeps_repository_filename_until_queries_finish(self) -> None:
        calls: list[list[str]] = []

        def fake_run(command, *, cwd, env, **kwargs):
            calls.append([str(item) for item in command])
            repository = Path(cwd)
            if command[0] == self.args.create:
                self.assertEqual(
                    (self.stage / "INSTALL").read_text(encoding="utf-8"),
                    self.install_hook.read_text(encoding="utf-8"),
                )
                self.assertEqual(
                    (self.stage / "REMOVE").read_text(encoding="utf-8"),
                    self.remove_hook.read_text(encoding="utf-8"),
                )
                (repository / "gitrun-server-1.9.1_1.x86_64.xbps").write_bytes(
                    b"xbps archive"
                )
                return SimpleNamespace(returncode=0, stdout="", stderr="")
            if command[0] == "tar" and "-tf" in command:
                return SimpleNamespace(
                    returncode=0,
                    stdout=(
                        "./usr/libexec/gitrun/gitrun-service\n"
                        "./etc/sv/gitrun/run\n"
                        "./etc/sv/gitrun/log/run\n"
                        "./etc/sv/gitrun-gsr/run\n"
                        "./etc/sv/gitrun-gsr/log/run\n"
                        "./INSTALL\n./REMOVE\n"
                    ),
                    stderr="",
                )
            if command[0] == "tar" and "-xOf" in command:
                hook = command[-1]
                body = "sv restart gitrun\n" if hook == "./INSTALL" else "sv down gitrun\n"
                return SimpleNamespace(returncode=0, stdout=body, stderr="")
            if command[0] == self.args.rindex:
                self.assertTrue(
                    (repository / "gitrun-server-1.9.1_1.x86_64.xbps").is_file()
                )
                return SimpleNamespace(returncode=0, stdout="", stderr="")
            if command[0] == self.args.query and "-x" in command:
                self.assertTrue(
                    (repository / "gitrun-server-1.9.1_1.x86_64.xbps").is_file()
                )
                return SimpleNamespace(returncode=0, stdout="glibc\n", stderr="")
            self.fail(f"Unexpected XBPS command: {command}")

        with patch.object(XBPS_BUILDER.subprocess, "run", side_effect=fake_run):
            result = XBPS_BUILDER.build_server_package(self.args)

        self.assertEqual(
            result,
            self.output / "GitRun-Server-v1.9.1-linux-x86_64-xbps.xbps",
        )
        self.assertTrue(result.is_file())
        self.assertEqual(len(list(self.output.glob("*.xbps"))), 1)
        dependency_queries = [
            call for call in calls if Path(call[0]).name == "xbps-query"
        ]
        self.assertEqual(len(dependency_queries), 1)
        self.assertTrue(any("-x" in call for call in dependency_queries))

    def test_rejects_package_without_remove_script_metadata(self) -> None:
        def fake_run(command, *, cwd, env, **kwargs):
            repository = Path(cwd)
            if command[0] == self.args.create:
                (repository / "gitrun-server-1.9.1_1.x86_64.xbps").write_bytes(
                    b"xbps archive"
                )
                return SimpleNamespace(returncode=0, stdout="", stderr="")
            if command[0] == "tar" and "-tf" in command:
                return SimpleNamespace(
                    returncode=0,
                    stdout="".join(f"{entry}\n" for entry in (
                        "./usr/libexec/gitrun/gitrun-service",
                        "./etc/sv/gitrun/run",
                        "./etc/sv/gitrun/log/run",
                        "./etc/sv/gitrun-gsr/run",
                        "./etc/sv/gitrun-gsr/log/run",
                        "./INSTALL",
                        "./REMOVE",
                    )),
                    stderr="",
                )
            if command[0] == "tar" and "-xOf" in command:
                body = "sv restart gitrun\n" if command[-1] == "./INSTALL" else ""
                return SimpleNamespace(returncode=0, stdout=body, stderr="")
            if command[0] == self.args.rindex:
                return SimpleNamespace(returncode=0, stdout="", stderr="")
            if command[0] == self.args.query and "-x" in command:
                return SimpleNamespace(returncode=0, stdout="glibc\n", stderr="")
            self.fail(f"Unexpected XBPS command: {command}")

        with patch.object(XBPS_BUILDER.subprocess, "run", side_effect=fake_run):
            with self.assertRaisesRegex(RuntimeError, "REMOVE metadata"):
                XBPS_BUILDER.build_server_package(self.args)

    def test_rejects_missing_lifecycle_hooks(self) -> None:
        self.install_hook.write_text("echo install\n", encoding="utf-8")
        with patch.object(
            XBPS_BUILDER.subprocess,
            "run",
            side_effect=AssertionError("XBPS must not run without valid hooks"),
        ):
            with self.assertRaises(RuntimeError):
                XBPS_BUILDER.build_server_package(self.args)


if __name__ == "__main__":
    unittest.main()
