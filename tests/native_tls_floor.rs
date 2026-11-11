//! The native-tls backend never offers a protocol below TLS 1.2, whatever the
//! platform permits (docs/DECISIONS.md D19), matching the rustls backend.
//!
//! Linux only: OpenSSL reads `OPENSSL_CONF`, so the test makes the host
//! permit TLS 1.0 and 1.1 (`MinProtocol = TLSv1`, `SECLEVEL=0`). What the
//! ClientHello then offers is the crate's own floor, not the platform's.
//! On macOS and Windows the OS refuses legacy TLS itself, which would mask
//! the setting. One test in its own binary, so the variable is set before
//! anything in the process initialises OpenSSL.

#![cfg(all(feature = "native-tls", target_os = "linux"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod transport_support;

use std::io::Write as _;
use std::time::Duration;

use agui_sdk::client::transport::{HttpTransport, TlsBackend, Transport};
use tokio::io::AsyncReadExt as _;
use transport_support::{block, request};

/// The `supported_versions` (extension 43) list of a TLS ClientHello record.
fn supported_versions(record: &[u8]) -> Vec<u16> {
    // Record header (5) and handshake header (4), then the ClientHello.
    let hello = &record[9..];
    let mut at = 2 + 32; // legacy_version, random
    at += 1 + usize::from(hello[at]); // session id
    at += 2 + usize::from(u16::from_be_bytes([hello[at], hello[at + 1]])); // suites
    at += 1 + usize::from(hello[at]); // compression methods
    let end = at + 2 + usize::from(u16::from_be_bytes([hello[at], hello[at + 1]]));
    at += 2;
    while at + 4 <= end {
        let kind = u16::from_be_bytes([hello[at], hello[at + 1]]);
        let len = usize::from(u16::from_be_bytes([hello[at + 2], hello[at + 3]]));
        let body = &hello[at + 4..at + 4 + len];
        if kind == 43 {
            return body[1..]
                .chunks(2)
                .map(|v| u16::from_be_bytes([v[0], v[1]]))
                .collect();
        }
        at += 4 + len;
    }
    Vec::new()
}

#[test]
fn native_tls_offers_nothing_below_tls_1_2() {
    let dir = std::env::temp_dir().join(format!("agui-native-tls-floor-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conf = dir.join("openssl.cnf");
    let mut file = std::fs::File::create(&conf).unwrap();
    file.write_all(
        b"openssl_conf = init\n[init]\nssl_conf = ssl\n[ssl]\nsystem_default = legacy\n\
          [legacy]\nMinProtocol = TLSv1\nCipherString = DEFAULT@SECLEVEL=0\n",
    )
    .unwrap();
    drop(file);
    std::env::set_var("OPENSSL_CONF", &conf);

    let offered = block(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let transport = HttpTransport::builder(format!("https://{addr}/"))
            .tls_backend(TlsBackend::NativeTls)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let accept = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = [0u8; 5];
            socket.read_exact(&mut head).await.unwrap();
            let len = usize::from(u16::from_be_bytes([head[3], head[4]]));
            let mut record = head.to_vec();
            record.resize(5 + len, 0);
            socket.read_exact(&mut record[5..]).await.unwrap();
            supported_versions(&record)
        });
        // The handshake fails once the listener drops the socket.
        let _ = transport.open(request()).await;
        accept.await.unwrap()
    });
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        offered.contains(&0x0303),
        "TLS 1.2 is offered: {offered:04x?}"
    );
    assert!(
        !offered.iter().any(|v| *v < 0x0303),
        "nothing below TLS 1.2 is offered: {offered:04x?}"
    );
}
