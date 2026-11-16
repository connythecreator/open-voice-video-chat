# Security policy

ovvc is pre-1.0 software. It has not had an independent security audit, and it
should not be treated as production-hardened until that changes.

## Supported versions

| Version | Supported |
| --- | --- |
| 0.1.x pre-release | Yes, on a best-effort basis |
| Anything older | No |

There is no long-term support branch yet. A security fix is made on the current
development line and noted in the changelog.

## Reporting a vulnerability

Please do not open a public issue, a public pull request or a public discussion
for a security problem. Report it privately through the repository's security
advisory feature:
<https://github.com/connythecreator/open-voice-video-chat/security/advisories/new>.

Include:

* The version or commit you tested.
* The platform, the build features and whether the daemon used the null or
  PipeWire backend.
* A clear description of the impact and the attacker's position, such as a
  remote peer, a member of a commune, another local user or a malicious stored
  event.
* Reproduction steps or a minimal proof of concept.
* Any logs, backtraces or packet captures that help. Remove your identity seed,
  message content and personal paths before you send them.

You will get an acknowledgement when the report is read, and an assessment
once the problem is reproduced. There is no formal response deadline, because
the project has no dedicated security team. Please allow time for a fix and a
release before you disclose the issue publicly.

## Scope

The following areas are in scope:

* Event signature verification, canonical encoding and the validation rules
  that run before an event is indexed.
* Control-frame verification, replay protection, rate limiting and the voice
  key exchange.
* Media encryption, key epochs, the replay window and the star hub re-seal
  path.
* The local IPC socket, its uid check and its frame limits.
* Display sanitisation for names, message bodies and log lines.
* Path handling for attachments, configuration files, profiles and themes.
* The libp2p transport, address hygiene and connection limits.

## Known limitations

These are not vulnerabilities by themselves, but they matter when you assess a
report:

* The project is pre-1.0 and has not been audited.
* Guild policy is a signed read model in v0.1. It is not enforced.
* Media keys rotate per epoch, not per message. There is no per-message
  ratchet and no explicit key confirmation.
* There is no plugin loader, so there is no plugin sandbox to attack or rely
  on.
* Video capture and screen sharing are not implemented.
* A commune replicates its full history to every member. Sharding and
  erasure-coded cold storage are future work.
* The default build uses the null audio backend. A live PipeWire build changes
  the device surface but not the media encryption boundary.
* The optional relay server forwards encrypted traffic. It is not a trust
  anchor and must not be treated as one.

## Design references

The security boundaries and the wire rules are described in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/PROTOCOL.md](docs/PROTOCOL.md)
and [docs/MEDIA.md](docs/MEDIA.md). A report that points at a specific boundary
is easier to assess than a general concern.
