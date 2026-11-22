# Third-party notices

Kinodisc is licensed under the GNU GPL, version 3 or later (see `LICENSE`).
It also contains material from the projects below, used under their own
licenses.

## tsMuxeR

- Project: tsMuxeR, <https://github.com/justdan96/tsMuxer> (version 2.7.0).
- License: Apache License 2.0. A copy is in `LICENSES/Apache-2.0-tsMuxer.txt`.
- Copyright: the tsMuxeR authors and contributors.

The native AVCHD/Blu-ray muxer (`crates/kinodisc-hdmv/src/bdav.rs` and
`crates/kinodisc-hdmv/src/native.rs`) writes the same on-disc structures as
tsMuxeR. A PS4 plays tsMuxeR's AVCHD discs and refused the ones Kinodisc
1.2.5 wrote, so Kinodisc now follows tsMuxeR's output. Taken from tsMuxeR:

- the AVCHD index extension data ("IDEX" block of `index.bdmv`), copied
  byte for byte (`AVCHD_INDEX_EXTENSION`);
- the Selection Information Table section (`SIT_SECTION`), copied byte for
  byte;
- the layout of the PMT HDMV and AC-3 descriptors, of the clip info
  (ClipInfo, SequenceInfo, ProgramInfo, CPI with its EP map) and of the
  playlist AppInfoPlayList, following tsMuxeR's `tsPacket.cpp` and
  `blurayHelper.cpp`. Kinodisc's code is its own Rust implementation.

Kinodisc changed the material above: it is used inside a different program,
and the EP map is built from FFmpeg's output instead of tsMuxeR's own muxer.

The test fixtures `crates/kinodisc-hdmv/tests/fixtures/tsmuxer-avchd-*` are
files tsMuxeR 2.7.0 wrote (a 24-second test clip), kept to check that
Kinodisc's output matches them byte for byte.
