# paste-assetgraph Roadmap

Deferred work the current architecture names but does not implement:

- **S3 storage backend** — `storage.backend` accepts only `local` and
  `dist`; an `s3` value is a configuration error today. See
  ARCHITECTURE.md → S3 Backend (Future).
- **Source map generation** — `source-maps` parses for compatibility but
  emits no `.map` files; the build reports a note when it is set `true`.
- **Image optimization pipeline** — `output.images` organizes the output
  directory; no transform or optimization step exists.
- **CloudFront invalidation** — listed in ARCHITECTURE.md → Future
  Enhancements; no implementation.
