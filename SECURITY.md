# Security

## Report a vulnerability privately

Use [GitHub private vulnerability reporting](https://github.com/givtaj/rusta-pitest/security/advisories/new)
for a suspected vulnerability. Do not put credentials, private response data, or
an unpatched exploit in a public issue. Include the affected version, operating
system, a minimal reproduction with synthetic data, the observed impact, and any
suggested mitigation. Avoid attaching raw benchmark reports before reviewing
URLs and other fields for sensitive data.

Ordinary bugs and usability questions belong in
[GitHub issues](https://github.com/givtaj/rusta-pitest/issues).

## Supported versions

Security fixes are made for the latest published alpha. Earlier alpha versions
may require upgrading rather than receiving a separate patch. Alpha APIs can
change, and there is no guaranteed response time or long-term support window.

## Operational scope

Run workloads only against services you own or are authorized to test. Start
with low concurrency and a small request count; repeated POSTs can mutate the
target's data. This runner does not isolate or reset the target service.

Saved request metadata omits URL credentials, query strings, fragments, and
request bodies, but URL paths remain visible. Review reports before sharing.
Command-line arguments and terminal output can also expose values to your local
environment. The runner has no custom-header or session-cookie support.

The release process audits locked dependencies and verifies artifact checksums.
A passing advisory scan reflects the database checked at that time; report newly
disclosed or otherwise undetected issues through the private channel above.
