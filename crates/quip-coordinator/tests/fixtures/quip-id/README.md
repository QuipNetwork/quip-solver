Byte-for-byte copies of id.quip.network's shared conformance fixtures
(`clients/js/test/fixtures/` in the id.quip.network repository). That repository
asserts these are exactly what its server answers today, so the stub in
`tests/common/quip_id.rs` speaks the real wire format.

Refresh them from there when the provider API changes; do not edit them here.
