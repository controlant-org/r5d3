use anyhow::Result;
use argh::FromArgs;
use aws_config::{
  default_provider::{credentials::DefaultCredentialsChain, region::DefaultRegionChain},
  sts::AssumeRoleProvider,
};
use aws_sdk_route53::model as rm;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_stream::StreamExt;

// tracing
use opentelemetry::{
  sdk::{
    trace::{self, RandomIdGenerator, Sampler},
    Resource,
  },
  KeyValue,
};
use opentelemetry_otlp::WithExportConfig;
use tracing::{debug, debug_span, info, info_span, instrument, trace_span, Instrument};
use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, FromArgs)]
/// DNS promoter
struct App {
  #[argh(switch)]
  /// run controller without actually performing any modifications
  dry_run: bool,

  #[argh(switch, short = 'o')]
  /// run controller just once
  once: bool,

  #[argh(option, short = 'd')]
  /// root domain for the controller
  root_domain: String,

  #[argh(option, short = 'r')]
  /// optional role to assume for the root domain, not needed if controller is run in the root domain account
  root_role: Option<String>,

  #[argh(option, short = 's')]
  /// roles to assume for subdomains
  sub_roles: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
  let tracer = opentelemetry_otlp::new_pipeline()
    .tracing()
    .with_exporter(
      opentelemetry_otlp::new_exporter()
        .tonic()
        .with_endpoint("http://localhost:4317")
        .with_timeout(Duration::from_secs(3)),
    )
    .with_trace_config(
      trace::config()
        .with_sampler(Sampler::AlwaysOn)
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(Resource::new(vec![KeyValue::new("service.name", "r5d3")])),
    )
    .install_batch(opentelemetry::runtime::Tokio)?;

  let tracer = tracing_opentelemetry::layer().with_tracer(tracer);

  let stdout_logger = tracing_subscriber::fmt::layer();

  tracing_subscriber::registry()
    .with(LevelFilter::INFO)
    .with(stdout_logger)
    .with(tracer)
    .init();

  let app: App = argh::from_env();
  debug!("loaded config: {:?}", app);

  loop {
    main_loop(&app).await?;

    if app.once {
      break;
    }

    sleep(Duration::from_secs(5 * 60)).await;
  }

  opentelemetry::global::shutdown_tracer_provider();

  Ok(())
}

#[instrument(skip_all)]
async fn main_loop(app: &App) -> Result<()> {
  let region = DefaultRegionChain::builder()
    .build()
    .region()
    .instrument(info_span!("loading default region"))
    .await
    .unwrap();

  let root_r53 = {
    if let Some(root_role) = app.root_role.as_ref() {
      info!(root_role, "using root role");
      let provider = AssumeRoleProvider::builder(root_role)
        .session_name("ctrl-cidr")
        .region(region.clone())
        .build(Arc::new(
          DefaultCredentialsChain::builder()
            .build()
            .instrument(trace_span!("build cred chain"))
            .await,
        ) as Arc<_>);

      let root_config = aws_config::from_env()
        .credentials_provider(provider)
        .load()
        .instrument(debug_span!("assume role for root domain"))
        .await;
      aws_sdk_route53::Client::new(&root_config)
    } else {
      info!("no root role");
      let base_aws_config = aws_config::load_from_env()
        .instrument(info_span!("load aws config from env"))
        .await;
      aws_sdk_route53::Client::new(&base_aws_config)
    }
  };

  let rid = root_r53
    .list_hosted_zones()
    .send()
    .instrument(info_span!("list hosted zones on root domain"))
    .await?
    .hosted_zones()
    .unwrap()
    .into_iter()
    .find(|hz| hz.name().unwrap().starts_with(&app.root_domain))
    .unwrap()
    .id()
    .unwrap()
    .to_string();

  info!(id = rid, "found root domain zone id");

  for sub_role in app.sub_roles.iter() {
    let provider = AssumeRoleProvider::builder(sub_role)
      .session_name("ctrl-cidr")
      .region(region.clone())
      .build(Arc::new(
        DefaultCredentialsChain::builder()
          .build()
          .instrument(info_span!("build cred chain"))
          .await,
      ) as Arc<_>);

    let sub_config = aws_config::from_env()
      .credentials_provider(provider)
      .load()
      .instrument(info_span!("build aws_config for subdomain", sub_role))
      .await;
    let sub_r53 = aws_sdk_route53::Client::new(&sub_config);
    let mut zones = sub_r53
      .list_hosted_zones()
      .into_paginator()
      .items()
      .send()
      .instrument(info_span!("fetch subdomain zones"));

    while let Some(Ok(zone)) = zones.inner_mut().next().await {
      // ignore private zones
      if zone.config().unwrap().private_zone() {
        debug!(name = zone.name(), id = zone.id(), "skipping private zone");
        continue;
      }

      // first ensure the zone is delegated to the root domain
      let nsrr: Vec<_> = sub_r53
        .get_hosted_zone()
        .id(zone.id().unwrap())
        .send()
        .instrument(info_span!("get subdomain delegation set", zone_name = zone.name()))
        .await?
        .delegation_set()
        .unwrap()
        .name_servers()
        .unwrap()
        .iter()
        .map(|ns| rm::ResourceRecord::builder().value(ns).build())
        .collect();

      let cb = rm::ChangeBatch::builder()
        .changes(
          rm::Change::builder()
            .action(rm::ChangeAction::Upsert)
            .resource_record_set(
              rm::ResourceRecordSet::builder()
                .r#type(rm::RrType::Ns)
                .name(zone.name().unwrap())
                .set_resource_records(Some(nsrr))
                .ttl(86400)
                .build(),
            )
            .build(),
        )
        .build();

      if app.dry_run {
        info!("would upsert NS record: {:?}", &cb);
      } else {
        root_r53
          .change_resource_record_sets()
          .hosted_zone_id(&rid)
          .change_batch(cb)
          .send()
          .instrument(info_span!("upsert NS record"))
          .await?;
      }
    }
  }

  Ok(())
}
