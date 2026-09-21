//! The loopback proxy a confined command's network goes through — sandbox
//! RFC §4.3, §7.
//!
//! ## Why the sandbox needs a proxy at all
//!
//! `allowedDomains` and `deniedDomains` are rules about *names*, and no
//! platform's isolation primitive can enforce one:
//!
//! * Linux under bubblewrap has no kernel-level domain filter at all.
//! * seatbelt expresses an address and a port; there is no hostname in the
//!   grammar.
//! * WFP is keyed on an account SID, and its filters run below the point
//!   where a name exists.
//!
//! So all three do the same thing: cut the network and route it through a
//! loopback listener that *does* see names. This crate is that listener. Until
//! it existed, configuring a domain rule turned the network off entirely and
//! nothing said so.
//!
//! ## No interception
//!
//! The proxy never decrypts anything. A `CONNECT` names its target in the
//! request line and a SOCKS5 `CONNECT` carries the hostname in the request
//! body, which is everything the decision needs. That choice removes an
//! entire subsystem: no generated CA, no root the user has to install, no
//! five environment variables pointing five TLS stacks at a certificate
//! bundle, and on macOS no `trustd` exception in the seatbelt profile. It
//! also means the proxy cannot see or log request contents, which is the
//! right default for something sitting between an agent and the internet.
//!
//! The cost is that filtering is per-connection and by name only: a rule
//! cannot say "this path on that host". That matches what the settings can
//! express.
//!
//! ## Shape
//!
//! ```text
//! DomainPolicy          pure decision — allow/deny by hostname
//!   ├─ http::parse      request head → destination (pure)
//!   ├─ socks::parse_*   greeting/request → destination (pure)
//!   └─ server::start    two loopback ports (+ two unix sockets on Linux)
//! ```
//!
//! Everything that decides is a pure function with a coverage matrix;
//! everything async is plumbing that asks it a question.

pub mod http;
pub mod policy;
pub mod server;
pub mod socks;

pub use policy::{DomainPolicy, Verdict};
pub use server::{start, start_blocking, ProxyEndpoints, ProxyError, ProxyHandle};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// A stand-in origin server that accepts one connection and reports what
    /// it was sent.
    async fn origin() -> (u16, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 1024];
            let read = stream.read(&mut buffer).await.unwrap_or(0);
            buffer.truncate(read);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi")
                .await
                .ok();
            buffer
        });
        (port, handle)
    }

    async fn proxy(policy: DomainPolicy) -> ProxyHandle {
        start(policy, None).await.unwrap()
    }

    async fn request_through(port: u16, request: &str) -> String {
        try_request_through(port, request)
            .await
            .expect("the proxy under test refused the connection")
    }

    /// [`request_through`] for the one test where not connecting is the
    /// expected outcome.
    ///
    /// Unwrapping the connect inside the shared helper turned that outcome
    /// into a panic: a closed listener refuses the connection outright on
    /// macOS, so the test asserting the proxy had *stopped* failed precisely
    /// because it had.
    async fn try_request_through(port: u16, request: &str) -> std::io::Result<String> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
        stream.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.ok();
        Ok(String::from_utf8_lossy(&response).into_owned())
    }

    #[tokio::test]
    async fn an_allowed_host_reaches_the_origin() {
        let (origin_port, origin_handle) = origin().await;
        // `localhost` is the name; the policy allows it, and the connection
        // is made to it.
        let handle = proxy(DomainPolicy::new(["localhost"], Vec::<String>::new())).await;

        let response = request_through(
            handle.endpoints().http_port,
            &format!("GET http://localhost:{origin_port}/x HTTP/1.1\r\n\r\n"),
        )
        .await;

        assert!(response.contains("200 OK"), "{response}");
        let seen = String::from_utf8(origin_handle.await.unwrap()).unwrap();
        assert!(seen.starts_with("GET /x HTTP/1.1\r\n"), "{seen}");
        assert!(seen.contains("Connection: close"), "{seen}");
    }

    #[tokio::test]
    async fn a_blocked_host_gets_a_403_that_names_the_rule() {
        // Not a dropped connection: that reaches the model as a timeout,
        // which reads like a broken network and gets retried for a minute.
        let handle = proxy(DomainPolicy::new(["allowed.test"], Vec::<String>::new())).await;

        let response = request_through(
            handle.endpoints().http_port,
            "GET http://blocked.test/x HTTP/1.1\r\n\r\n",
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
        assert!(response.contains("Rebon sandbox"), "{response}");
        assert!(response.contains("allowed.test"), "{response}");
    }

    #[tokio::test]
    async fn a_blocked_connect_is_refused_before_any_tunnel_opens() {
        let handle = proxy(DomainPolicy::new(["allowed.test"], Vec::<String>::new())).await;

        let response = request_through(
            handle.endpoints().http_port,
            "CONNECT blocked.test:443 HTTP/1.1\r\n\r\n",
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(
            !response.contains("200 Connection Established"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn a_denied_host_is_refused_even_with_no_allow_list() {
        let handle = proxy(DomainPolicy::new(Vec::<String>::new(), ["blocked.test"])).await;

        let response = request_through(
            handle.endpoints().http_port,
            "GET http://blocked.test/x HTTP/1.1\r\n\r\n",
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    }

    #[tokio::test]
    async fn a_lying_host_header_does_not_get_past_the_check() {
        // The check is made on the host that will be dialled, and the
        // rewritten head carries that same host upstream.
        let handle = proxy(DomainPolicy::new(["allowed.test"], Vec::<String>::new())).await;

        let response = request_through(
            handle.endpoints().http_port,
            "GET http://blocked.test/x HTTP/1.1\r\nHost: allowed.test\r\n\r\n",
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    }

    #[tokio::test]
    async fn userinfo_cannot_disguise_the_target() {
        let handle = proxy(DomainPolicy::new(["allowed.test"], Vec::<String>::new())).await;

        let response = request_through(
            handle.endpoints().http_port,
            "GET http://allowed.test@blocked.test/x HTTP/1.1\r\n\r\n",
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    }

    #[tokio::test]
    async fn a_malformed_request_gets_a_400_rather_than_a_hang() {
        let handle = proxy(DomainPolicy::default()).await;

        let response = request_through(handle.endpoints().http_port, "NONSENSE\r\n\r\n").await;

        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    }

    #[tokio::test]
    async fn socks_refuses_a_blocked_domain_with_the_ruleset_code() {
        let handle = proxy(DomainPolicy::new(["allowed.test"], Vec::<String>::new())).await;
        let mut stream = TcpStream::connect(("127.0.0.1", handle.endpoints().socks_port))
            .await
            .unwrap();

        stream.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method = [0u8; 2];
        stream.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [0x05, 0x00]);

        let mut request = vec![0x05, 0x01, 0x00, 0x03, 12];
        request.extend_from_slice(b"blocked.test");
        request.extend_from_slice(&443u16.to_be_bytes());
        stream.write_all(&request).await.unwrap();

        let mut reply = [0u8; 10];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], socks::REPLY_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn socks_lets_an_allowed_domain_through() {
        let (origin_port, origin_handle) = origin().await;
        let handle = proxy(DomainPolicy::new(["localhost"], Vec::<String>::new())).await;
        let mut stream = TcpStream::connect(("127.0.0.1", handle.endpoints().socks_port))
            .await
            .unwrap();

        stream.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method = [0u8; 2];
        stream.read_exact(&mut method).await.unwrap();

        let mut request = vec![0x05, 0x01, 0x00, 0x03, 9];
        request.extend_from_slice(b"localhost");
        request.extend_from_slice(&origin_port.to_be_bytes());
        stream.write_all(&request).await.unwrap();

        let mut reply = [0u8; 10];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], socks::REPLY_SUCCESS);

        stream.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        let seen = String::from_utf8(origin_handle.await.unwrap()).unwrap();
        assert!(seen.starts_with("GET / HTTP/1.1"), "{seen}");
    }

    #[tokio::test]
    async fn socks_refuses_an_unsupported_method_rather_than_proceeding() {
        let handle = proxy(DomainPolicy::default()).await;
        let mut stream = TcpStream::connect(("127.0.0.1", handle.endpoints().socks_port))
            .await
            .unwrap();

        // Only username/password offered; the proxy speaks no-auth only.
        stream.write_all(&[0x05, 0x01, 0x02]).await.unwrap();
        let mut method = [0u8; 2];
        stream.read_exact(&mut method).await.unwrap();

        assert_eq!(method, [0x05, socks::METHOD_UNACCEPTABLE]);
    }

    #[tokio::test]
    async fn the_two_listeners_get_different_ephemeral_ports() {
        // Fixed ports would collide between two Rebon sessions on one
        // machine, and would let anything else on the box find the proxy by
        // guessing.
        let handle = proxy(DomainPolicy::default()).await;
        let endpoints = handle.endpoints();

        assert_ne!(endpoints.http_port, endpoints.socks_port);
        assert_ne!(endpoints.http_port, 0);
        assert_ne!(endpoints.socks_port, 0);
    }

    #[tokio::test]
    async fn dropping_the_handle_stops_accepting() {
        let handle = proxy(DomainPolicy::default()).await;
        let port = handle.endpoints().http_port;
        drop(handle);

        // The listener socket is closed with the handle; a connect either
        // fails or the request goes unanswered. Either is "stopped"; a
        // successful 403 would mean the session's proxy outlived it.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            try_request_through(port, "GET http://x.test/ HTTP/1.1\r\n\r\n"),
        )
        .await;
        let served = matches!(&outcome, Ok(Ok(response)) if response.contains("403"));
        assert!(
            !served,
            "the proxy kept serving after its handle was dropped"
        );
    }

    #[tokio::test]
    async fn an_unconfigured_policy_still_serves_but_allows_everything() {
        let (origin_port, origin_handle) = origin().await;
        let handle = proxy(DomainPolicy::default()).await;

        let response = request_through(
            handle.endpoints().http_port,
            &format!("GET http://localhost:{origin_port}/ HTTP/1.1\r\n\r\n"),
        )
        .await;

        assert!(response.contains("200 OK"), "{response}");
        let _ = origin_handle.await;
    }

    #[tokio::test]
    async fn an_oversized_head_is_refused_rather_than_buffered_forever() {
        let handle = proxy(DomainPolicy::default()).await;
        let mut stream = TcpStream::connect(("127.0.0.1", handle.endpoints().http_port))
            .await
            .unwrap();

        // Never sends the terminator.
        let filler = format!("X-Pad: {}\r\n", "a".repeat(8192));
        for _ in 0..12 {
            if stream.write_all(filler.as_bytes()).await.is_err() {
                break;
            }
        }
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_to_end(&mut response),
        )
        .await;

        let text = String::from_utf8_lossy(&response);
        assert!(text.contains("400"), "expected a refusal, got {text:?}");
    }

    #[test]
    fn the_policy_is_shareable_across_connections() {
        // Every connection reads the same compiled policy; rebuilding it per
        // connection would let two connections in one session disagree.
        let policy = Arc::new(DomainPolicy::new(["example.com"], Vec::<String>::new()));
        let clone = policy.clone();
        assert_eq!(policy.decide("example.com"), clone.decide("example.com"));
    }
}
