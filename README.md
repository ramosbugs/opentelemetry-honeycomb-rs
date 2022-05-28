# OpenTelemetry Exporter for Honeycomb (Unofficial)

[<img alt="github" src="https://img.shields.io/badge/github-ramosbugs/opentelemetry--honeycomb--rs-8da0cb?style=for-the-badge&labelColor=555555&logo=github" height="20"/>](https://github.com/ramosbugs/opentelemetry-honeycomb-rs)
[<img alt="crates.io" src="https://img.shields.io/crates/v/opentelemetry-honeycomb.svg?style=for-the-badge&color=fc8d62&logo=rust" height="20"/>](https://crates.io/crates/opentelemetry-honeycomb)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-opentelemetry--honeycomb-66c2a5?style=for-the-badge&labelColor=555555&logoColor=white&logo=data:image/svg+xml;base64,PHN2ZyByb2xlPSJpbWciIHhtbG5zPSJodHRwOi8vd3d3LnczLm9yZy8yMDAwL3N2ZyIgdmlld0JveD0iMCAwIDUxMiA1MTIiPjxwYXRoIGZpbGw9IiNmNWY1ZjUiIGQ9Ik00ODguNiAyNTAuMkwzOTIgMjE0VjEwNS41YzAtMTUtOS4zLTI4LjQtMjMuNC0zMy43bC0xMDAtMzcuNWMtOC4xLTMuMS0xNy4xLTMuMS0yNS4zIDBsLTEwMCAzNy41Yy0xNC4xIDUuMy0yMy40IDE4LjctMjMuNCAzMy43VjIxNGwtOTYuNiAzNi4yQzkuMyAyNTUuNSAwIDI2OC45IDAgMjgzLjlWMzk0YzAgMTMuNiA3LjcgMjYuMSAxOS45IDMyLjJsMTAwIDUwYzEwLjEgNS4xIDIyLjEgNS4xIDMyLjIgMGwxMDMuOS01MiAxMDMuOSA1MmMxMC4xIDUuMSAyMi4xIDUuMSAzMi4yIDBsMTAwLTUwYzEyLjItNi4xIDE5LjktMTguNiAxOS45LTMyLjJWMjgzLjljMC0xNS05LjMtMjguNC0yMy40LTMzLjd6TTM1OCAyMTQuOGwtODUgMzEuOXYtNjguMmw4NS0zN3Y3My4zek0xNTQgMTA0LjFsMTAyLTM4LjIgMTAyIDM4LjJ2LjZsLTEwMiA0MS40LTEwMi00MS40di0uNnptODQgMjkxLjFsLTg1IDQyLjV2LTc5LjFsODUtMzguOHY3NS40em0wLTExMmwtMTAyIDQxLjQtMTAyLTQxLjR2LS42bDEwMi0zOC4yIDEwMiAzOC4ydi42em0yNDAgMTEybC04NSA0Mi41di03OS4xbDg1LTM4Ljh2NzUuNHptMC0xMTJsLTEwMiA0MS40LTEwMi00MS40di0uNmwxMDItMzguMiAxMDIgMzguMnYuNnoiPjwvcGF0aD48L3N2Zz4K" height="20"/>](https://docs.rs/opentelemetry-honeycomb)
[<img alt="build status" src="https://img.shields.io/travis/ramosbugs/opentelemetry-honeycomb-rs/main?style=for-the-badge" height="20"/>](https://travis-ci.org/github/ramosbugs/opentelemetry-honeycomb-rs)

[Documentation](https://docs.rs/opentelemetry-honeycomb)

## Getting Started

```rust
use std::sync::Arc;

use async_executors::TokioTpBuilder;
use opentelemetry::trace::Tracer;
use opentelemetry::global::shutdown_tracer_provider;
use opentelemetry_honeycomb::HoneycombApiKey;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
  let mut builder = TokioTpBuilder::new();
  builder
      .tokio_builder()
      .enable_time()
      .enable_io();
  let executor = Arc::new(builder.build().expect("Failed to build Tokio executor"));

    // Create a new instrumentation pipeline.
    let (_flusher, tracer) = opentelemetry_honeycomb::new_pipeline(
        HoneycombApiKey::new(
            std::env::var("HONEYCOMB_API_KEY")
                .expect("Missing or invalid environment variable HONEYCOMB_API_KEY")
        ),
        std::env::var("HONEYCOMB_DATASET")
            .expect("Missing or invalid environment variable HONEYCOMB_DATASET"),
        executor.clone(),
        move |fut| executor.block_on(fut),
    )
    .install()?;

    tracer.in_span("doing_work", |cx| {
        // Traced app logic here...
    });

    shutdown_tracer_provider();
    Ok(())
}
```
