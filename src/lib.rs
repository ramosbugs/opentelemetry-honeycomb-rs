//! OpenTelemetry Exporter for Honeycomb (Unofficial)
//!
//! This crate implements an [OpenTelemetry Span Exporter](https://github.com/open-telemetry/opentelemetry-specification/blob/master/specification/trace/sdk.md#span-exporter)
//! for [Honeycomb](https://honeycomb.io) that plugs into the [`opentelemetry`] crate.
//!
//! # Getting Started
//!
//! Please see the [`opentelemetry`] documentation for general OpenTelemetry usage. The example
//! below illustrates how the Honeycomb exporter can be used.
//!
//! ### Example
//! ```rust,no_run
//! use async_executors::TokioTpBuilder;
//! use opentelemetry::trace::Tracer;
//! use opentelemetry_honeycomb::HoneycombApiKey;
//!
//! use std::sync::Arc;
//!
//! fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
//!     let mut builder = TokioTpBuilder::new();
//!     builder
//!         .tokio_builder()
//!         .enable_io();
//!     let executor = Arc::new(builder.build().expect("Failed to build Tokio executor"));
//!
//!     // Create a new instrumentation pipeline.
//!     //
//!     // NOTE: uninstalling the tracer happens when the _uninstall variable is dropped. Assigning
//!     // it to _ will immediately drop it and uninstall the tracer!
//!     let (_flusher, tracer, _uninstall) = opentelemetry_honeycomb::new_pipeline(
//!         HoneycombApiKey::new(
//!             std::env::var("HONEYCOMB_API_KEY")
//!                 .expect("Missing or invalid environment variable HONEYCOMB_API_KEY")
//!         ),
//!         std::env::var("HONEYCOMB_DATASET")
//!             .expect("Missing or invalid environment variable HONEYCOMB_DATASET"),
//!         executor.clone(),
//!         move |fut| executor.block_on(fut),
//!     ).install().expect("Failed to install OpenTelemetry pipeline");
//!
//!     tracer.in_span("doing_work", |cx| {
//!         // Traced app logic here...
//!     });
//!
//!     Ok(())
//! }
//! ```
use async_std::sync::RwLock;
use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use derivative::Derivative;
use futures::future::BoxFuture;
use hazy::OpaqueDebug;
use libhoney::transmission::Transmission;
use libhoney::{Client, Event, FieldHolder, Value};
use log::{debug, error, trace, warn};
use opentelemetry::global::TracerProviderGuard;
use opentelemetry::sdk::export::trace::{ExportResult, SpanData, SpanExporter};
use opentelemetry::sdk::export::ExportError;
use opentelemetry::sdk::Resource;
use opentelemetry::trace::{SpanId, StatusCode, TraceError, TraceId, TracerProvider};
use opentelemetry::{Array, KeyValue};
use thiserror::Error;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Produced by libhoney:
pub use libhoney::Error as HoneycombError;
pub use libhoney::FutureExecutor;

/// Honeycomb API key.
///
/// The [`std::fmt::Debug`] implementation of this type redacts the key, so it is safe to embed in
/// other data structures.
#[derive(Clone, OpaqueDebug)]
pub struct HoneycombApiKey(String);
impl HoneycombApiKey {
    pub fn new(api_key: String) -> Self {
        Self(api_key)
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn secret(&self) -> &str {
        &self.0
    }
}

/// Create a new exporter pipeline builder.
pub fn new_pipeline<B>(
    api_key: HoneycombApiKey,
    dataset: String,
    executor: FutureExecutor,
    block_on: B,
) -> HoneycombPipelineBuilder
where
    B: Fn(BoxFuture<()>) + Send + Sync + 'static,
{
    HoneycombPipelineBuilder {
        api_key,
        block_on: Arc::new(block_on),
        dataset,
        executor,
        trace_config: None,
        transmission_options: libhoney::transmission::Options {
            user_agent_addition: Some(format!(
                "{}-rs/{}",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            )),
            ..Default::default()
        },
    }
}

/// Pipeline builder
#[derive(Derivative)]
#[derivative(Debug)]
pub struct HoneycombPipelineBuilder {
    api_key: HoneycombApiKey,
    #[derivative(Debug = "ignore")]
    block_on: Arc<dyn Fn(BoxFuture<()>) + Send + Sync>,
    dataset: String,
    #[derivative(Debug = "ignore")]
    executor: FutureExecutor,
    trace_config: Option<opentelemetry::sdk::trace::Config>,
    transmission_options: libhoney::transmission::Options,
}
impl HoneycombPipelineBuilder {
    /// Assign the SDK trace configuration.
    pub fn with_trace_config(mut self, config: opentelemetry::sdk::trace::Config) -> Self {
        self.trace_config = Some(config);
        self
    }

    /// Sets the number of events to collect into a batch before sending to Honeycomb.
    pub fn with_max_batch_size(mut self, max_batch_size: usize) -> Self {
        self.transmission_options.max_batch_size = max_batch_size;
        self
    }

    /// Sets the number of batches that can be inflight to Honeycomb simultaneously.
    pub fn with_max_concurrent_batches(mut self, max_concurrent_batches: usize) -> Self {
        self.transmission_options.max_concurrent_batches = max_concurrent_batches;
        self
    }

    /// Specifies how often to send batches to Honeycomb.
    pub fn with_batch_timeout(mut self, batch_timeout: Duration) -> Self {
        self.transmission_options.batch_timeout = batch_timeout;
        self
    }

    /// Install the Honeycomb exporter pipeline with the recommended defaults.
    pub fn install(
        mut self,
    ) -> Result<
        (
            HoneycombFlusher,
            opentelemetry::sdk::trace::Tracer,
            Uninstall,
        ),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let client = Arc::new(RwLock::new(Some(libhoney::init(libhoney::Config {
            executor: self.executor,
            options: libhoney::client::Options {
                api_key: self.api_key.into_inner(),
                dataset: self.dataset,
                ..Default::default()
            },
            transmission_options: self.transmission_options,
        })?)));

        let exporter = HoneycombSpanExporter {
            block_on: self.block_on,
            client: client.clone(),
        };

        // Libhoney implements its own batching, so we just use the simple exporter here instead of
        // double-batching (which would require double-flushing, etc.).
        let mut provider_builder =
            opentelemetry::sdk::trace::TracerProvider::builder().with_simple_exporter(exporter);
        if let Some(config) = self.trace_config.take() {
            provider_builder = provider_builder.with_config(config);
        }
        let provider = provider_builder.build();
        let tracer = provider.get_tracer(
            "opentelemetry-honeycomb-rs",
            Some(env!("CARGO_PKG_VERSION")),
        );
        let provider_guard = opentelemetry::global::set_tracer_provider(provider);

        Ok((
            HoneycombFlusher { client },
            tracer,
            Uninstall(provider_guard),
        ))
    }
}

#[derive(Clone)]
pub struct HoneycombFlusher {
    client: Arc<RwLock<Option<Client<Transmission>>>>,
}
impl HoneycombFlusher {
    pub async fn flush(&self) -> Result<(), HoneycombExporterError> {
        log::debug!("Flushing Honeycomb client");
        let mut guard = self.client.write().await;
        guard
            .as_mut()
            .ok_or(HoneycombExporterError::Shutdown)?
            .flush()
            .await
            .map_err(HoneycombExporterError::Honeycomb)
    }
}

#[derive(Debug, Error)]
pub enum HoneycombExporterError {
    #[error("Honeycomb error")]
    Honeycomb(#[source] HoneycombError),
    #[error("exporter is already shut down")]
    Shutdown,
}
impl ExportError for HoneycombExporterError {
    fn exporter_name(&self) -> &'static str {
        "honeycomb"
    }
}

fn timestamp_from_system_time(ts: SystemTime) -> DateTime<Utc> {
    Utc.timestamp_millis(
        ts.duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_millis() as u64 as i64,
    )
}

fn otel_value_to_serde_json(value: opentelemetry::Value) -> Value {
    match value {
        opentelemetry::Value::Bool(val) => Value::Bool(val),
        opentelemetry::Value::I64(val) => Value::Number(val.into()),
        opentelemetry::Value::F64(val) => {
            if let Some(v) = serde_json::Number::from_f64(val) {
                Value::Number(v)
            } else {
                Value::Null
            }
        }
        opentelemetry::Value::Array(Array::Bool(vals)) => {
            Value::Array(vals.into_iter().map(Value::Bool).collect())
        }
        opentelemetry::Value::Array(Array::F64(vals)) => Value::Array(
            vals.into_iter()
                .flat_map(serde_json::Number::from_f64)
                .map(Value::Number)
                .collect(),
        ),
        opentelemetry::Value::Array(Array::I64(vals)) => {
            Value::Array(vals.into_iter().map(|v| Value::Number(v.into())).collect())
        }
        opentelemetry::Value::Array(Array::String(vals)) => Value::Array(
            vals.into_iter()
                .map(|v| Value::String(v.into_owned()))
                .collect(),
        ),
        opentelemetry::Value::String(val) => Value::String(val.into_owned()),
    }
}

/// Port of https://github.com/honeycombio/opentelemetry-exporter-python/blob/133ab6d7c5362ee24e4277264cf9a7634a5f6394/opentelemetry/ext/honeycomb/__init__.py#L36.
#[derive(Derivative)]
#[derivative(Debug)]
struct HoneycombSpanExporter {
    #[derivative(Debug = "ignore")]
    client: Arc<RwLock<Option<Client<Transmission>>>>,
    #[derivative(Debug = "ignore")]
    block_on: Arc<dyn Fn(BoxFuture<()>) + Send + Sync>,
}
impl HoneycombSpanExporter {
    fn new_trace_event<I>(
        client: &Client<Transmission>,
        start_time: SystemTime,
        trace_id: TraceId,
        parent_id: SpanId,
        attributes: I,
        resource: &Resource,
    ) -> Event
    where
        I: IntoIterator<Item = (opentelemetry::Key, opentelemetry::Value)>,
    {
        let mut event = client.new_event();
        let timestamp = timestamp_from_system_time(start_time);
        event.set_timestamp(timestamp);
        // The Honeycomb Python exporter sets start_time even though it seems to get ignored by
        // Honeycomb.
        event.add_field(
            "start_time",
            Value::String(timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)),
        );
        event.add_field("trace.trace_id", Value::String(trace_id.to_hex()));

        // From the Honeycomb docs:
        //   "The root span for any given trace must have no field for “parentId” in its event. If
        //    all of the spans in a trace have a “parentId”, Honeycomb will not show a root span for
        //    that trace."
        if parent_id != SpanId::invalid() {
            event.add_field("trace.parent_id", Value::String(parent_id.to_hex()));
        }

        for (attr_name, attr_value) in resource
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .chain(attributes.into_iter())
        {
            event.add_field(attr_name.as_str(), otel_value_to_serde_json(attr_value))
        }

        event
    }
}

#[async_trait]
impl SpanExporter for HoneycombSpanExporter {
    async fn export(&mut self, batch: Vec<SpanData>) -> ExportResult {
        debug!("Exporting batch of {} spans", batch.len());
        for span in batch {
            let client_guard = self.client.read().await;
            let client = client_guard
                .as_ref()
                .ok_or(HoneycombExporterError::Shutdown)?;
            let mut event = Self::new_trace_event(
                &client,
                span.start_time,
                span.span_context.trace_id(),
                span.parent_span_id,
                span.attributes,
                &span.resource,
            );
            event.add_field(
                "trace.span_id",
                Value::String(span.span_context.span_id().to_hex()),
            );
            event.add_field("name", Value::String(span.name.clone()));
            if let Ok(duration_ms) = span.end_time.duration_since(span.start_time) {
                event.add_field(
                    "duration_ms",
                    Value::Number((duration_ms.as_millis() as u64).into()),
                );
            }
            event.add_field(
                "response.status_code",
                Value::Number((span.status_code as i32).into()),
            );
            event.add_field("status.message", Value::String(span.status_message.clone()));
            event.add_field("span.kind", Value::String(format!("{}", span.span_kind)));

            if !matches!(span.status_code, StatusCode::Unset) {
                event.add_field(
                    "error",
                    Value::Bool(!matches!(span.status_code, StatusCode::Ok)),
                );
            }

            trace!("Sending Honeycomb event: {:#?}", event);
            event.send(&client).await.map_err(|err| {
                TraceError::ExportFailed(Box::new(HoneycombExporterError::Honeycomb(err)))
            })?;

            for span_event in span.message_events.into_iter() {
                let mut event = Self::new_trace_event(
                    &client,
                    span_event.timestamp,
                    span.span_context.trace_id(),
                    // The parent of the event is the current span, as opposed to the parent of the span,
                    // which is some other span (unless it's the root span).
                    span.span_context.span_id(),
                    span_event
                        .attributes
                        .into_iter()
                        .map(|KeyValue { key, value }| (key, value)),
                    &span.resource,
                );
                event.add_field("duration_ms", Value::Number(0.into()));
                event.add_field("name", Value::String(span_event.name));
                event.add_field(
                    "meta.annotation_type",
                    Value::String("span_event".to_string()),
                );

                trace!("Sending Honeycomb event: {:#?}", event);
                event.send(&client).await.map_err(|err| {
                    TraceError::ExportFailed(Box::new(HoneycombExporterError::Honeycomb(err)))
                })?;
            }

            if !span.links.is_empty() {
                // FIXME: support links
                warn!("Links are not yet supported");
            }
        }
        Ok(())
    }

    /// Shuts down the exporter.
    ///
    /// This function will panic if called from within an async context.
    fn shutdown(&mut self) {
        debug!("Shutting down HoneycombSpanExporter");

        let client = self.client.clone();
        (self.block_on)(Box::pin(async move {
            let mut guard = client.write().await;
            if let Some(client) = guard.take() {
                client.close().await.unwrap_or_else(|err| {
                    error!("Failed to shut down HoneycombSpanExporter: {}", err);
                });
            }
        }));
    }
}

impl Drop for HoneycombSpanExporter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Uninstalls the Honeycomb pipeline on drop.
#[derive(Debug)]
pub struct Uninstall(TracerProviderGuard);
