use std::{sync::Arc, time::Duration};

use anyhow::Result;
use aws_config::{
  default_provider::{credentials::DefaultCredentialsChain, region::DefaultRegionChain},
  sts::AssumeRoleProvider,
};
use aws_sdk_route53::types as rm;
use clap::Parser;
use tokio::time::sleep;
use tokio_stream::StreamExt;
use tracing::{debug, debug_span, info, info_span, instrument, trace_span, warn, Instrument};

/// DNS promoter
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct App {
  /// run controller without actually performing any modifications
  #[arg(long)]
  dry_run: bool,

  /// run controller just once
  #[arg(short, long)]
  once: bool,

  /// root domain for the controller
  #[arg(short = 'd', long)]
  root_domain: String,

  /// optional role to assume for the root domain, not needed if controller is run in the root domain account
  #[arg(short, long)]
  root_role: Option<String>,

  #[arg(short, long = "sub-role")]
  /// roles to assume for subdomains (sub accounts)
  sub_roles: Vec<String>,

  /// AWS regions to check ACM certificates in
  #[arg(short, long = "acm-region")]
  acm_regions: Option<Vec<String>>,
}

mod acm;
mod o11y;

#[tokio::main]
async fn main() -> Result<()> {
  o11y::setup();

  let app = App::parse();
  debug!("loaded config: {:?}", app);

  loop {
    main_loop(&app).await?;

    if app.once {
      break;
    }

    sleep(Duration::from_secs(5 * 60)).await;
  }

  o11y::teardown();

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
    let span = info_span!("sub_role", role = sub_role);
    let _guard = span.enter();

    let mut subdomains = Vec::new();

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

      subdomains.push(zone.name().unwrap().trim_end_matches('.').to_string());

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
        warn!("would upsert NS record: {:?}", &cb);
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

    let cbs = if let Some(ref acm_regions) = app.acm_regions {
      let mut ret = Vec::new();
      for ar in acm_regions {
        use aws_types::region::Region;

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
          .region(Region::new(ar.clone()))
          .load()
          .instrument(info_span!("build aws_config for subdomain", sub_role))
          .await;
        let sub_acm = aws_sdk_acm::Client::new(&sub_config);

        let vals = acm::find_validations(sub_acm, &app.root_domain, &subdomains).await?;
        ret.extend(vals);
      }

      ret
    } else {
      let sub_acm = aws_sdk_acm::Client::new(&sub_config);

      acm::find_validations(sub_acm, &app.root_domain, &subdomains).await?
    };

    for cb in cbs {
      if app.dry_run {
        warn!("would upsert DNS validation record: {:?}", &cb);
      } else {
        root_r53
          .change_resource_record_sets()
          .hosted_zone_id(&rid)
          .change_batch(cb)
          .send()
          .instrument(info_span!("upsert DNS validation record"))
          .await?;
      }
    }
  }

  Ok(())
}
