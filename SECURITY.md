# Security Policy

This is a demonstrator project, but we treat security hygiene as first-class.

## Reporting a vulnerability

Please report suspected vulnerabilities privately via GitHub Security Advisories
("Report a vulnerability" on the Security tab) rather than a public issue. We aim
to acknowledge within a few working days.

## Secrets

- Never commit secrets. `.env` is gitignored; only `.env.example` (with empty
  values) is tracked.
- All model/embedding access uses environment-provided keys through the
  provider-neutral boundary: no keys in source, config, or fixtures.

## Supply chain

- Dependencies are checked with `cargo-deny` (advisories, licenses, bans, sources).
