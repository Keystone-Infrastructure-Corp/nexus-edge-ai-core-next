---
name: defender-holds-test-binary-udp
description: "On a Mac running Microsoft Defender's network extension, UDP from freshly built test binaries can be held, even on loopback; rerun and compare with nc before debugging"
metadata:
  type: project
---

On a workstation with Microsoft Defender's Network Extension installed
(`systemextensionsctl list` shows `com.microsoft.wdav.netext`), the filter can
intermittently hold UDP flows from freshly built Rust test binaries. Seen so far:

- a SIP INVITE with 0 bytes transmitted (`netstat -anvb -p udp`, `txbytes` column)
- RTP sends that returned `Ok` 18 times and then never became writable
- loopback-only SIP unit tests failing (11 of 25) until the same binary was rerun, when
  25 of 25 passed

`nc`, `curl`, `/usr/bin/python3` and `ffmpeg` (Apple-signed or long-installed) were not
affected. Homebrew python was blocked outright.

**Why:** three "fixes" in a row were tested against a host that could not give a clean
signal. Only per-socket `netstat` byte counters separated code faults from the filter.

**How to apply:**

- When a UDP or socket test fails on a Mac, first rerun the same binary and check the
  peer with `nc`. Read `netstat -anvb -p udp` columns as `$3` Send-Q, `$6` rxbytes,
  `$7` txbytes, `$10` process.
- Prove hardware behaviour from a Linux host (an edge box, or a Colima container built
  with `git archive` per `linux-clippy-in-colima`), not from macOS test binaries.
- One real code-level finding survived the noise: a speaker's inbound RTP arriving
  before our first outbound packet let the filter hold the flow. Priming the RTP flow
  at answer fixed it.
