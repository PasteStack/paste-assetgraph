# Copyright (c) 2017–present Tomshley LLC
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     https://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

import os
import pathlib
import re

from setuptools import setup
from setuptools.command.sdist import sdist


# VERSION is the single source of the released version, as it is for every other
# project in the family; CI checks a release tag against the same file. It lives
# at the repository root here; a copy beside setup.py wins because the sdist
# carries one written there (see _VersionedSdist below).
def _base_version() -> str:
    here = pathlib.Path(__file__).parent
    for candidate in (here / "VERSION", here / ".." / ".." / "VERSION"):
        if candidate.is_file():
            return candidate.read_text().strip()
    raise FileNotFoundError("no VERSION file beside setup.py or at the repository root")


_BASE_VERSION = _base_version()
_VALID_CHANNELS = {"pinnable", "rolling", "tag"}
_PRERELEASE = re.compile(r"(\.dev\d+|a\d+|b\d+|rc\d+|\.post\d+)$")


def compose_version(base_version: str = _BASE_VERSION) -> str:
    revision = os.environ.get("TOMSHLEY_CICD_BUILD_REVISION", "").strip()
    channel = os.environ.get("TOMSHLEY_CICD_BUILD_CHANNEL", "").strip().lower()
    if channel and channel not in _VALID_CHANNELS:
        raise ValueError(f"unsupported TOMSHLEY_CICD_BUILD_CHANNEL: {channel}")
    if channel == "tag" or not revision:
        return base_version
    version = base_version if _PRERELEASE.search(base_version) else f"{base_version}.dev0"
    segments = ".".join(f"commit{segment}" if segment.isdigit() else segment for segment in revision.split("-"))
    # The local-version prefix keeps a rolling build sorting above the pinnable
    # build of the same revision, so "latest" wins a version comparison.
    prefix = "z" if channel == "rolling" else "a"
    return f"{version}+{prefix}.{segments}"


def distribution_version() -> str:
    """The version this build of the package carries.

    An unpacked sdist pins the exact version it was built with through the
    generated BUILD_VERSION beside setup.py — rebuilding it without the CI
    revision in the environment must not silently lose it.
    """
    frozen = pathlib.Path(__file__).parent / "BUILD_VERSION"
    return frozen.read_text().strip() if frozen.is_file() else compose_version()


class _VersionedSdist(sdist):
    """Write the resolved versions into the release tree.

    MANIFEST.in cannot name a file outside the project directory, so the sdist
    gets VERSION and BUILD_VERSION beside setup.py from here — a wheel built
    from the unpacked sdist then resolves the same base version and the same
    channel/revision-qualified version the sdist itself carried.
    """

    def make_release_tree(self, base_dir, files):
        super().make_release_tree(base_dir, files)
        (pathlib.Path(base_dir) / "VERSION").write_text(_BASE_VERSION + "\n")
        (pathlib.Path(base_dir) / "BUILD_VERSION").write_text(
            self.distribution.get_version() + "\n"
        )


setup(version=distribution_version(), cmdclass={"sdist": _VersionedSdist})
