A controller for 'promoting' domains in AWS Route53. Scenario:

- You have a root domain managed in Route53 in account A - e.g. `example.com`
- You have various other accounts managing subdomains in Route53 - e.g. `foo.example.com` and `bar.example.com`
- You want to map a root domain record to a subdomain record - e.g. `x.example.com` should CNAME to `x.foo.example.com`

Normally, an operator (or CI) would need access to manage Route53 in multiple accounts to automate this. This controller alleviates these extra access, which can be annoying and tricky to manage, by managing 3 things:

- NS records to setup delegation for subdomains on the root domain
- CNAME records on the root domain to point to records in specific subdomain
- TXT records to complete DNS-based validation for ACM certificates that serve both the root and subdomain domains

The controller can be configured with filters on both record level and subdomain level, which should help to minimize disruption by misconfiguration.

# Configuration

- TODO for each account to promote subdomains from, create IAM role with these policy and allow assume role

# Misc

The name is a tribute to R2D2 by mashing together `Route53` and `DNS`.
