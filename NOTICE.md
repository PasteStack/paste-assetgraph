# NOTICE

## paste-assetgraph

Copyright 2025–present Tomshley LLC

This product includes software developed and maintained by Tomshley LLC as part
of the PasteStack project.

---

## License

This project is licensed under the **Apache License, Version 2.0**.

The Apache License includes an explicit patent grant and patent retaliation
clause. The full license text is available in the `LICENSE` file at the root of
this repository.

---

## Trademarks

**Tomshley** and the **Tomshley logo** are trademarks of Tomshley LLC.

Use of these trademarks is governed by applicable trademark law and Tomshley
brand usage guidelines. Brand assets are **not** licensed under the Apache
License 2.0.

---

## Third-Party Notices

The `paste-assetgraph` CLI binaries — including the copies the sbt plugin
ships — and the native library bundled with the Node, Python, and JVM
runtimes statically link third-party Rust crates under their own licenses
(MIT, Apache-2.0, MPL-2.0, BSD, ISC, and others). `THIRD_PARTY_LICENSES.md`,
shipped beside this file, lists every such crate with its version, its
source location, and its license texts. Source for each crate, including the
MPL-2.0 ones, is available from crates.io at the listed version. The same file
carries the notices of the components the toolchain links in: the Rust
standard library and the crates it links in every binary, musl libc in the
static Linux binaries, and in the Windows binaries the runtime the Zig
linker adds: Zig's libc and compiler_rt, LLVM libunwind and the MinGW-w64
runtime.

The JVM libraries' own dependencies are resolved from their registries under
their own licenses; the sbt build files record the versions in use.
