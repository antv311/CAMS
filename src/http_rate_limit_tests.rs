async fn start_rate_limited_http(
    rate_limit: crate::rate_limit::RateLimitConfig,
    logger: Option<crate::logging::EventForwarder>,
) -> Result<(Server, String, tokio::task::JoinHandle<()>)> {
    let db = store::open(":memory:")?;
    let server = match logger {
        Some(forwarder) => Server::with_logger(db, forwarder),
        None => Server::new(db),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mut info = build_runtime_info("127.0.0.1:6667", &addr.to_string(), None);
    info.rate_limit = rate_limit;
    let app = router(server.clone(), info);
    let task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    Ok((server, format!("http://{addr}"), task))
}

#[tokio::test]
async fn ip_rate_limit_enforces_burst_headers_429_and_refill() -> Result<()> {
    let (_, base, task) = start_rate_limited_http(
        crate::rate_limit::RateLimitConfig {
            enabled: true,
            ip_requests_per_sec: 4.0,
            ip_burst: 3,
            principal_requests_per_sec: 100.0,
            principal_burst: 100,
            trust_proxy_headers: false,
            max_buckets: 100,
        },
        None,
    )
    .await?;

    let client = reqwest::Client::new();
    for expected_rem in [2, 1, 0] {
        let resp = client.get(format!("{base}/v1/agents")).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("x-ratelimit-limit").unwrap(), "3");
        assert_eq!(
            resp.headers().get("x-ratelimit-remaining").unwrap(),
            &expected_rem.to_string()
        );
        assert_eq!(resp.headers().get("x-ratelimit-scope").unwrap(), "ip");
    }

    let rejected = client.get(format!("{base}/v1/agents")).send().await?;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(rejected.headers().get("retry-after").unwrap(), "1");
    assert_eq!(rejected.headers().get("x-ratelimit-remaining").unwrap(), "0");
    assert_eq!(rejected.headers().get("x-ratelimit-scope").unwrap(), "ip");
    let body: Value = rejected.json().await?;
    assert_eq!(body["ok"], false);
    assert_eq!(body["limit_scope"], "ip");
    assert_eq!(body["retry_after_seconds"], 1);

    // OPTIONS preflight bypasses rate limiting.
    let preflight = client
        .request(reqwest::Method::OPTIONS, format!("{base}/v1/groups/any"))
        .send()
        .await?;
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);

    // At 4 req/s, 300ms recovers > 1 token.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let recovered = client.get(format!("{base}/v1/agents")).send().await?;
    assert_eq!(recovered.status(), StatusCode::OK);

    task.abort();
    Ok(())
}

#[tokio::test]
async fn principal_rate_limit_stops_runaway_agent_without_starving_localhost_peers() -> Result<()> {
    let (server, base, task) = start_rate_limited_http(
        crate::rate_limit::RateLimitConfig {
            enabled: true,
            ip_requests_per_sec: 100.0,
            ip_burst: 50,
            principal_requests_per_sec: 0.5,
            principal_burst: 2,
            trust_proxy_headers: false,
            max_buckets: 100,
        },
        None,
    )
    .await?;

    let (alpha, alpha_tok_1) = server
        .auth()
        .register_principal("alpha", Some("member"), None)?;
    let alpha_tok_2 = server
        .auth()
        .issue_token_with_options(&alpha.id, Some("second"), false, None, None)?
        .token;
    let (_, beta_tok) = server
        .auth()
        .register_principal("beta", Some("member"), None)?;

    let client = reqwest::Client::new();

    // Alpha spends 1 token on token #1 and 1 token on token #2 (shared principal bucket).
    let r1 = client
        .get(format!("{base}/v1/me"))
        .bearer_auth(&alpha_tok_1)
        .send()
        .await?;
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(r1.headers().get("x-ratelimit-scope").unwrap(), "principal");
    assert_eq!(r1.headers().get("x-ratelimit-remaining").unwrap(), "1");

    let r2 = client
        .get(format!("{base}/v1/me"))
        .bearer_auth(&alpha_tok_2)
        .send()
        .await?;
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(r2.headers().get("x-ratelimit-remaining").unwrap(), "0");

    // Alpha's runaway loop hits 60 consecutive 429s without draining localhost IP burst (50).
    for _ in 0..60 {
        let rej = client
            .get(format!("{base}/v1/me"))
            .bearer_auth(&alpha_tok_1)
            .send()
            .await?;
        assert_eq!(rej.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(rej.headers().get("x-ratelimit-scope").unwrap(), "principal");
        let body: Value = rej.json().await?;
        assert_eq!(body["limit_scope"], "principal");
    }

    // Beta on the same 127.0.0.1 IP still succeeds with its full principal burst.
    for expected_rem in [1, 0] {
        let ok = client
            .get(format!("{base}/v1/me"))
            .bearer_auth(&beta_tok)
            .send()
            .await?;
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(
            ok.headers().get("x-ratelimit-remaining").unwrap(),
            &expected_rem.to_string()
        );
    }

    task.abort();
    Ok(())
}

#[tokio::test]
async fn rate_limit_invalid_tokens_and_proxy_headers() -> Result<()> {
    // Default trust_proxy_headers = false ignores spoofed X-Forwarded-For.
    let (_, base_untrusted, task_untrusted) = start_rate_limited_http(
        crate::rate_limit::RateLimitConfig {
            enabled: true,
            ip_requests_per_sec: 0.5,
            ip_burst: 2,
            principal_requests_per_sec: 10.0,
            principal_burst: 10,
            trust_proxy_headers: false,
            max_buckets: 100,
        },
        None,
    )
    .await?;

    let client = reqwest::Client::new();
    for idx in 0..2 {
        let resp = client
            .get(format!("{base_untrusted}/v1/me"))
            .bearer_auth("airc_bogus_token_value")
            .header("x-forwarded-for", format!("203.0.113.{}", idx + 1))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    let blocked = client
        .get(format!("{base_untrusted}/v1/me"))
        .bearer_auth("airc_bogus_token_value")
        .header("x-forwarded-for", "203.0.113.99")
        .send()
        .await?;
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(blocked.headers().get("x-ratelimit-scope").unwrap(), "ip");
    task_untrusted.abort();

    // Opt-in trust_proxy_headers = true isolates buckets by forwarded client IP.
    let (_, base_trusted, task_trusted) = start_rate_limited_http(
        crate::rate_limit::RateLimitConfig {
            enabled: true,
            ip_requests_per_sec: 0.5,
            ip_burst: 1,
            principal_requests_per_sec: 10.0,
            principal_burst: 10,
            trust_proxy_headers: true,
            max_buckets: 100,
        },
        None,
    )
    .await?;

    let first_ip = client
        .get(format!("{base_trusted}/v1/agents"))
        .header("x-forwarded-for", "198.51.100.10, 10.0.0.1")
        .send()
        .await?;
    assert_eq!(first_ip.status(), StatusCode::OK);
    let first_ip_blocked = client
        .get(format!("{base_trusted}/v1/agents"))
        .header("x-forwarded-for", "198.51.100.10")
        .send()
        .await?;
    assert_eq!(first_ip_blocked.status(), StatusCode::TOO_MANY_REQUESTS);

    let second_ip = client
        .get(format!("{base_trusted}/v1/agents"))
        .header("x-forwarded-for", "198.51.100.11")
        .send()
        .await?;
    assert_eq!(second_ip.status(), StatusCode::OK);

    task_trusted.abort();
    Ok(())
}

#[tokio::test]
async fn rate_limit_rejection_emits_throttled_security_audit_and_can_be_disabled() -> Result<()> {
    let syslog_rx = std::net::UdpSocket::bind("127.0.0.1:0")?;
    syslog_rx.set_nonblocking(true)?;
    let syslog_addr = syslog_rx.local_addr()?;

    let forwarder = crate::logging::EventForwarder::new(crate::logging::LogConfig {
        syslog_target: Some(crate::logging::SyslogTarget::Udp(syslog_addr.to_string())),
        syslog_format: crate::logging::SyslogFormat::Rfc5424,
        stderr_errors: false,
        ..crate::logging::LogConfig::default()
    })?;

    let (server, base, task) = start_rate_limited_http(
        crate::rate_limit::RateLimitConfig {
            enabled: true,
            ip_requests_per_sec: 10.0,
            ip_burst: 10,
            principal_requests_per_sec: 0.5,
            principal_burst: 1,
            trust_proxy_headers: false,
            max_buckets: 100,
        },
        Some(forwarder),
    )
    .await?;

    let (_, token) = server
        .auth()
        .register_principal("noisy", Some("member"), None)?;
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{base}/v1/me"))
            .bearer_auth(&token)
            .send()
            .await?
            .status(),
        StatusCode::OK
    );
    for _ in 0..5 {
        assert_eq!(
            client
                .get(format!("{base}/v1/me"))
                .bearer_auth(&token)
                .send()
                .await?
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    server.logger().flush();

    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok((len, _)) = syslog_rx.recv_from(&mut buf) {
        let msg = String::from_utf8_lossy(&buf[..len]).into_owned();
        if msg.contains("http.rate_limited") {
            received.push(msg);
        }
    }
    assert_eq!(
        received.len(),
        1,
        "expected throttled http.rate_limited audit event, got {received:?}"
    );
    assert!(received[0].contains("scope=principal"));
    assert!(received[0].contains("@noisy"));
    task.abort();

    // Disabled rate limiting passes bursts without X-RateLimit-* headers.
    let (_, disabled_base, disabled_task) = start_rate_limited_http(
        crate::rate_limit::RateLimitConfig {
            enabled: false,
            ip_requests_per_sec: 0.5,
            ip_burst: 1,
            principal_requests_per_sec: 0.5,
            principal_burst: 1,
            trust_proxy_headers: false,
            max_buckets: 100,
        },
        None,
    )
    .await?;
    for _ in 0..5 {
        let resp = client.get(format!("{disabled_base}/v1/agents")).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("x-ratelimit-limit").is_none());
    }
    disabled_task.abort();
    Ok(())
}
