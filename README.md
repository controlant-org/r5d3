A controller for automating domains in AWS Route53 for multi-account scenario:

- You have a root domain managed in Route53 in account A - e.g. `example.com`
- You have various other accounts managing subdomains in Route53 - e.g. `foo.example.com` and `bar.example.com`
- You want to map a root domain record to a subdomain record - e.g. `x.example.com` should CNAME to `x.foo.example.com`
- You want to provision TLS certificates through AWS ACM

Normally, an operator (or CI) would need access to manage Route53 in multiple accounts to automate this. This controller alleviates these extra access, which can be annoying and tricky to manage, by managing these things:

- NS records to setup delegation for subdomains on the root domain
- CNAME records to complete DNS-based validation for ACM certificates that serve both the root and subdomain domains

# Configuration

TODO root account policy

TODO for each account to promote subdomains from, create IAM role with these policy and allow assume role

# Misc

The name is a tribute to R2D2 by mashing together `Route53` and `DNS`.
