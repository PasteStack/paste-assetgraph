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
