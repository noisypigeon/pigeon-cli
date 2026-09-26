# Terraform

Two independent trees:

- [`modules/`](modules/) — versioned, reusable Terraform modules
  (DigitalOcean, Scaleway). See [`modules/README.md`](modules/README.md)
  for the module index, versioning, and how to consume them.
- [`infrastructure/`](infrastructure/) — this repo owner's actual, live
  Terragrunt/Terraform configuration for personal infrastructure,
  consuming the modules above. See
  [`infrastructure/README.md`](infrastructure/README.md).

Neither is run from the other. `infrastructure/` pins module versions via
tagged `git::` sources, the same way any other consumer of `modules/`
would — see [ADR-0052](../docs/adr/0052-merge-pigeon-do-terraform-infrastructure.md)
for how `infrastructure/` came to live in this repo.
