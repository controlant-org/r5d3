use anyhow::Result;
use aws_sdk_route53::model as rm;
use tokio_stream::StreamExt;
use tracing::{info_span, instrument, Instrument};

#[instrument(skip_all)]
pub async fn find_validations(
  acm: aws_sdk_acm::Client,
  root_domain: &str,
  subdomains: &[String],
) -> Result<Vec<rm::ChangeBatch>> {
  let mut cbs = Vec::new();

  let mut certs = acm
    .list_certificates()
    .into_paginator()
    .items()
    .send()
    .instrument(info_span!("list ACM certificates"));

  while let Some(Ok(cert)) = certs.inner_mut().next().await {
    let val: Vec<_> = acm
      .describe_certificate()
      .certificate_arn(cert.certificate_arn().unwrap())
      .send()
      .instrument(info_span!(
        "describe certificate",
        arn = cert.certificate_arn().unwrap()
      ))
      .await?
      .certificate()
      .unwrap()
      .domain_validation_options()
      .unwrap()
      .iter()
      .cloned()
      .collect();

    for v in val {
      let domain = v.domain_name().unwrap();
      if subdomains.iter().find(|s| domain.ends_with(*s)).is_none() && domain.ends_with(root_domain) {
        let rr = v.resource_record().unwrap();

        let cb = rm::ChangeBatch::builder()
          .changes(
            rm::Change::builder()
              .action(rm::ChangeAction::Upsert)
              .resource_record_set(
                rm::ResourceRecordSet::builder()
                  .r#type(rr.r#type().unwrap().as_str().into())
                  .name(rr.name().unwrap())
                  .resource_records(rm::ResourceRecord::builder().value(rr.value().unwrap()).build())
                  .ttl(86400)
                  .build(),
              )
              .build(),
          )
          .build();

        cbs.push(cb);
      }
    }
  }

  Ok(cbs)
}
