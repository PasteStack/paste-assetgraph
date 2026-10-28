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

//! The paste-assetgraph library: the CLI binary and the FFI crate share
//! these modules. `api` is the single in-process entry point every render
//! or combo call funnels through; the library itself runs no server —
//! a host framework turns the `combo` response description into HTTP.

pub mod api;
pub mod config;
mod dirfd;
pub mod graph;
pub mod jam;
pub mod manifest;
pub mod storage;
