# ag-ui-sdk

An alias of [`agui-sdk`](https://crates.io/crates/agui-sdk), the Rust SDK for
the [AG-UI](https://docs.ag-ui.com) 1.0 protocol: protocol types plus a
conformant client and server over Server-Sent Events and protobuf.

This crate re-exports `agui-sdk` unchanged (`pub use agui_sdk::*`), with the
same features and the same version, released in lockstep. It exists so that
either spelling of the name finds the SDK. Prefer depending on `agui-sdk`
directly; the code, the documentation and the feature table live there:

- Documentation and feature flags: <https://docs.rs/agui-sdk>
- Source, changelog and security policy: <https://github.com/MattJackson/agui-sdk>

```toml
[dependencies]
# A client that calls agents over HTTPS (`rustls` implies `client`):
ag-ui-sdk = { version = "0.9", features = ["rustls"] }
```

In code the crate is `ag_ui_sdk`, with the same paths as `agui_sdk`
(`ag_ui_sdk::client::AgentClient`, `ag_ui_sdk::server::Agent`, and so on).

If you depend on both names, keep them at the same version: `ag-ui-sdk`
pins `agui-sdk` exactly (`=X.Y.Z`), so a mismatched pair fails to resolve.

This is an independent implementation, not affiliated with or endorsed by
the AG-UI project. MSRV: Rust 1.75 for every feature except `native-tls`, which needs 1.80. License: MIT, see
[`LICENSE`](https://github.com/MattJackson/agui-sdk/blob/main/LICENSE).
