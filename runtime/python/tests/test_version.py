import os
import re
import runpy
import sys
import types
import unittest
import unittest.mock

HERE = os.path.dirname(os.path.abspath(__file__))
RUNTIME_ROOT = os.path.dirname(HERE)
REPO_ROOT = os.path.normpath(os.path.join(RUNTIME_ROOT, "..", ".."))


def load_setup_namespace(setup_py=None):
    """Execute setup.py with setuptools stubbed out.

    The version helpers are pure, so the drift guard runs them without
    installing setuptools: the returned namespace carries `_base_version`,
    `compose_version` and `distribution_version`. `setup_py` defaults to the
    runtime's setup.py; a copy under a temp directory exercises the sdist's
    frozen-version path.
    """
    setuptools = types.ModuleType("setuptools")
    setuptools.setup = lambda **kwargs: None
    command = types.ModuleType("setuptools.command")
    sdist_module = types.ModuleType("setuptools.command.sdist")

    class _Sdist:
        pass

    sdist_module.sdist = _Sdist
    command.sdist = sdist_module
    setuptools.command = command
    stubs = {
        "setuptools": setuptools,
        "setuptools.command": command,
        "setuptools.command.sdist": sdist_module,
    }
    saved = {name: sys.modules.get(name) for name in stubs}
    sys.modules.update(stubs)
    try:
        return runpy.run_path(setup_py or os.path.join(RUNTIME_ROOT, "setup.py"))
    finally:
        for name, module in saved.items():
            if module is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = module


class VersionTest(unittest.TestCase):
    # VERSION is the single source of the released version: the release flow
    # bumps VERSION, Cargo.toml and runtime/node/package.json together; this
    # test, the Node test and the lint-rust CI check catch a missed one.

    def test_setup_base_version_matches_repository_version(self):
        with open(os.path.join(REPO_ROOT, "VERSION"), encoding="utf-8") as fh:
            expected = fh.read().strip()
        namespace = load_setup_namespace()
        self.assertEqual(namespace["_base_version"](), expected)
        with unittest.mock.patch.dict(
            os.environ,
            {"TOMSHLEY_CICD_BUILD_CHANNEL": "", "TOMSHLEY_CICD_BUILD_REVISION": ""},
        ):
            self.assertEqual(namespace["compose_version"](), expected)

    def test_compose_version_appends_channel_revision(self):
        namespace = load_setup_namespace()
        with unittest.mock.patch.dict(
            os.environ,
            {
                "TOMSHLEY_CICD_BUILD_CHANNEL": "pinnable",
                "TOMSHLEY_CICD_BUILD_REVISION": "feature-x-abc123",
            },
        ):
            self.assertEqual(
                namespace["compose_version"]("0.2.0"), "0.2.0.dev0+a.feature.x.abc123"
            )

    def test_sdist_freezes_the_composed_version(self):
        # A wheel built from an unpacked sdist must carry the version the
        # sdist itself was built with, whatever the environment now says.
        import shutil
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            shutil.copy(
                os.path.join(RUNTIME_ROOT, "setup.py"),
                os.path.join(tmp, "setup.py"),
            )
            with open(os.path.join(tmp, "VERSION"), "w", encoding="utf-8") as fh:
                fh.write("0.2.0\n")
            with open(
                os.path.join(tmp, "BUILD_VERSION"), "w", encoding="utf-8"
            ) as fh:
                fh.write("0.2.0.dev0+a.feature.x.abc123\n")

            namespace = load_setup_namespace(os.path.join(tmp, "setup.py"))
            # No CI environment at all: the frozen value wins.
            with unittest.mock.patch.dict(
                os.environ,
                {"TOMSHLEY_CICD_BUILD_CHANNEL": "", "TOMSHLEY_CICD_BUILD_REVISION": ""},
            ):
                self.assertEqual(
                    namespace["distribution_version"](),
                    "0.2.0.dev0+a.feature.x.abc123",
                )
            # A different branch build's environment must not leak in either.
            with unittest.mock.patch.dict(
                os.environ,
                {
                    "TOMSHLEY_CICD_BUILD_CHANNEL": "rolling",
                    "TOMSHLEY_CICD_BUILD_REVISION": "other-999",
                },
            ):
                self.assertEqual(
                    namespace["distribution_version"](),
                    "0.2.0.dev0+a.feature.x.abc123",
                )

    def test_source_tree_composes_the_version_anew(self):
        # Without a generated BUILD_VERSION the source tree composes from
        # VERSION plus the environment, as before.
        with open(os.path.join(REPO_ROOT, "VERSION"), encoding="utf-8") as fh:
            base = fh.read().strip()
        namespace = load_setup_namespace()
        with unittest.mock.patch.dict(
            os.environ,
            {
                "TOMSHLEY_CICD_BUILD_CHANNEL": "pinnable",
                "TOMSHLEY_CICD_BUILD_REVISION": "feature-x-abc123",
            },
        ):
            self.assertEqual(
                namespace["distribution_version"](),
                f"{base}.dev0+a.feature.x.abc123",
            )

    def test_pyproject_declares_dynamic_version(self):
        with open(
            os.path.join(RUNTIME_ROOT, "pyproject.toml"), encoding="utf-8"
        ) as fh:
            pyproject = fh.read()
        self.assertIn('dynamic = ["version"]', pyproject)
        self.assertIsNone(re.search(r"(?m)^version\s*=", pyproject))


if __name__ == "__main__":
    unittest.main()
