//! Optional OpenTelemetry integration (feature = "otel").
//!
//! Enable: `cargo build --features otel`
//! Configure via env:
//!   OTEL_EXPORTER_OTLP_ENDPOINT — e.g. http://localhost:4317
//!   OTEL_SERVICE_NAME           — defaults to "memoria"

#[cfg(feature = "otel")]
pub fn init_tracing() {
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_sdk::runtime;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let otlp = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .expect("OTLP exporter");

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(otlp, runtime::Tokio)
        .build();

    let tracer = provider.tracer("memoria");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .with(otel_layer)
        .init();
}

#[cfg(not(feature = "otel"))]
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
}
