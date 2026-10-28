# Support

## Usage and troubleshooting

For usage questions, configuration help and reproducible defects, open an issue using the closest available template. Include:

- GitRun source version or installed release version.
- Operating system and relevant platform details.
- The command or operation attempted and the observed result.
- A minimal, sanitized log excerpt and the expected behavior.

Do not include GitHub tokens, credentials, private keys, environment files, secret values or other sensitive data. Redact private hostnames and repository details when they are not needed to reproduce the issue.

## Security reports

Do not report vulnerabilities in public issues. Use the private vulnerability-reporting route provided by GitHub for this repository and consult [SECURITY.md](SECURITY.md).

## Technical references

- [Architecture](docs/ARCHITECTURE.md)
- [Git*Run workflow API](docs/GITRUN_API.md)
- [VM-backed runners](docs/VM.md)
- [IPC and socket architecture](docs/IPC.md)
- [Security model](docs/SECURITY_MODEL.md)
- [Kitty Smart Scheduler](docs/SMART_SCHEDULER.md)
- [CLI diagnostics and recovery](crates/gitrun-cli/README.md#json-integrity-and-recovery)
