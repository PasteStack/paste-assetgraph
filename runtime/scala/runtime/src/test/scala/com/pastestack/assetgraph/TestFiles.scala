// Copyright (c) 2017–present Tomshley LLC
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

package com.pastestack.assetgraph

import java.net.URLClassLoader
import java.nio.charset.StandardCharsets.UTF_8
import java.nio.file.{Files, Path}
import java.util.Comparator
import scala.util.Using

/** Temporary files for specs: every directory is deleted after its body runs. */
object TestFiles {

  def withTempDir[T](prefix: String)(f: Path => T): T = {
    val dir = Files.createTempDirectory(prefix)
    try f(dir) finally deleteTree(dir)
  }

  /** Files.walk holds directory handles open until its stream is closed. */
  def deleteTree(dir: Path): Unit =
    Using.resource(Files.walk(dir))(_.sorted(Comparator.reverseOrder()).forEach(Files.delete(_)))

  /** Run `f` with a context classloader that sees only `root`; the previous loader is restored. */
  def withContextClassLoader[T](root: Path)(f: => T): T =
    Using.resource(new URLClassLoader(Array(root.toUri.toURL), ClassLoader.getPlatformClassLoader)) { cl =>
      val thread = Thread.currentThread()
      val previous = thread.getContextClassLoader
      thread.setContextClassLoader(cl)
      try f finally thread.setContextClassLoader(previous)
    }

  /** Write `content` as UTF-8, creating parent directories. */
  def write(path: Path, content: String): Path = {
    Files.createDirectories(path.getParent)
    Files.write(path, content.getBytes(UTF_8))
  }

  /** Write a compiled output named by a manifest `output` path under `baseDir`. */
  def writeOutput(baseDir: Path, output: String, content: String): Path =
    write(baseDir.resolve(output.stripPrefix("/")), content)
}
