use anyhow::Result;
use argh::FromArgs;
use aws_config::{default_provider::credentials::DefaultCredentialsChain, sts::AssumeRoleProvider};
use aws_sdk_route53::model as rm;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_stream::StreamExt;
use tracing::{info, span, Level};

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
  tracing_subscriber::fmt::init();
  let app: App = argh::from_env();
  info!("loaded config: {:?}", app);

  loop {
    let base_aws_config = aws_config::load_from_env().await;
    let region = base_aws_config.region().unwrap().clone();

    let root_r53 = {
      let _ = span!(Level::INFO, "configuring root route53 client").entered();

      if let Some(root_role) = app.root_role.as_ref() {
        info!(root_role, "using root role");
        let provider = AssumeRoleProvider::builder(root_role)
          .session_name("ctrl-cidr")
          .region(region.clone())
          .build(Arc::new(DefaultCredentialsChain::builder().build().await) as Arc<_>);

        let root_config = aws_config::from_env().credentials_provider(provider).load().await;
        aws_sdk_route53::Client::new(&root_config)
      } else {
        info!("no root role");
        aws_sdk_route53::Client::new(&base_aws_config)
      }
    };

    let rid = root_r53
      .list_hosted_zones()
      .send()
      .await?
      .hosted_zones()
      .unwrap()
      .into_iter()
      .find(|hz| hz.name().unwrap().starts_with(&app.root_domain))
      .unwrap()
      .id()
      .unwrap()
      .to_string();

    for sub_role in app.sub_roles.iter() {
      let provider = AssumeRoleProvider::builder(sub_role)
        .session_name("ctrl-cidr")
        .region(region.clone())
        .build(Arc::new(DefaultCredentialsChain::builder().build().await) as Arc<_>);

      let sub_config = aws_config::from_env().credentials_provider(provider).load().await;
      let sub_r53 = aws_sdk_route53::Client::new(&sub_config);
      let mut zones = sub_r53.list_hosted_zones().into_paginator().items().send();

      while let Some(Ok(zone)) = zones.next().await {
        // ignore private zones
        if zone.config().unwrap().private_zone() {
          continue;
        }

        // first ensure the zone is delegated to the root domain
        let nsrr: Vec<_> = sub_r53
          .get_hosted_zone()
          .id(zone.id().unwrap())
          .send()
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
            .await?;
        }
      }

      //
    }

    if app.once {
      break;
    } else {
      sleep(Duration::from_secs(5 * 60)).await;
    }
  }

  Ok(())
}
