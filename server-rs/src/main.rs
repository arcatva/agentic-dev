use agentic_dev_server::{
    api,
    api::config::Config,
    api::state::{self, AppState},
    api::throttle::LoginThrottle,
    api::tls::TlsMode,
    engine::{self, providers, push, sdk_runner, store, transcript, Engine, EngineConfig},
};
use parking_lot::Mutex;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Minimal CLI surface: all real configuration is env-driven (see README).
    match std::env::args().nth(1).as_deref() {
        Some("--version") | Some("-V") => {
            println!("agentic-dev-server {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help") | Some("-h") => {
            println!(
                "agentic-dev-server {} — agent-driven local dev platform (HTTP + WebSocket API)\n\n\
                 Usage: agentic-dev-server [--version|--help]\n\n\
                 Configuration is environment-driven:\n\
                 \x20 AGENTIC_PASSWORD        login password (required for real use)\n\
                 \x20 AGENTIC_PORT            listen port (default 7420)\n\
                 \x20 AGENTIC_HOST            bind address (default 0.0.0.0)\n\
                 \x20 AGENTIC_SRC_ROOT        repos root (default ~/src)\n\
                 \x20 AGENTIC_MAX_CONCURRENT  optional cap on concurrent sessions (unset = unlimited)\n\
                 \x20 AGENTIC_NODE_BIN        node binary for the SDK bridge (default: node)\n\
                 \x20 AGENTIC_SDK_BRIDGE      path override for sdk-bridge.mjs\n\n\
                 HTTPS (on by default — self-signed cert auto-generated on first boot):\n\
                 \x20 AGENTIC_TLS             set to off/0/false to serve plain HTTP instead\n\
                 \x20 AGENTIC_TLS_CERT        bring-your-own PEM cert chain (needs AGENTIC_TLS_KEY)\n\
                 \x20 AGENTIC_TLS_KEY         bring-your-own PEM private key\n\
                 \x20 AGENTIC_TLS_DIR         where the self-signed cert+key live (default: <data>/tls)\n\
                 \x20 AGENTIC_TLS_SAN         extra cert SANs (IP or DNS), comma-separated\n\
                 \x20 AGENTIC_TLS_REGEN       set to 1/true to force-regenerate the self-signed cert\n\
                 \x20                         (download the active cert at GET /api/tls/cert.pem)\n\n\
                 Requires: node (>=18) with the bridge deps installed next to sdk-bridge.mjs,\n\
                 and the `claude` CLI on PATH, authenticated.",
                env!("CARGO_PKG_VERSION")
            );
            return;
        }
        Some(other) => {
            eprintln!("unknown argument: {other} (try --help)");
            std::process::exit(2);
        }
        None => {}
    }
    // Structured logging: honour AGENTIC_LOG_LEVEL, then RUST_LOG, default "info"
    // (bare fmt::init() ignored both — env-filter feature was off).
    let filter = tracing_subscriber::EnvFilter::try_from_env("AGENTIC_LOG_LEVEL")
        .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .compact()
        .init();
    install_panic_hook();
    let config = Arc::new(Config::load(|k| std::env::var(k).ok()));
    // Loud warning when running with the insecure defaults — these
    // are a guessable LAN password and a forgeable HMAC secret.
    if config.password == "changeme" {
        tracing::warn!("default password 'changeme' in use — set AGENTIC_PASSWORD");
    }
    if config.auth_secret == "dev-insecure-secret" {
        tracing::warn!("default authSecret in use — set AGENTIC_AUTH_SECRET");
    }
    let store = Arc::new(
        store::Store::open(config.db_path.clone(), config.log_dir.clone())
            .await
            .expect("open store"),
    );
    // Fetch Claude model IDs from the Anthropic API once at startup (non-blocking via std::thread).
    // On failure (ccswitch / no auth / network), the model list stays empty — native Claude models
    // are omitted from the model selector UI until the next restart with a valid credential.
    std::thread::spawn(providers::init_claude_models);
    let transcript = Arc::new(transcript::TranscriptCache::new(
        config.transcript_cache_bytes,
    ));
    // Phase 6: build the real push_fn closure. Load device token + creds fresh on each fire;
    // failures are swallowed inside send_push.
    let device_token_path = config.device_token_path.clone();
    let push_fn: Option<engine::PushFn> =
        Some(std::sync::Arc::new(move |payload: serde_json::Value| {
            let device_token_path = device_token_path.clone();
            let token = push::load_device_token(&device_token_path).map(|r| r.token);
            let creds = push::load_fcm_creds(|k| std::env::var(k).ok());
            let pp = push::PushPayload {
                session_id: payload
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                status: payload
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                is_error: payload
                    .get("isError")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                error_text: payload
                    .get("errorText")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                cost_usd: payload.get("costUsd").and_then(|v| v.as_f64()),
                title: payload
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            };
            tokio::spawn(async move {
                push::send_push(&pp, token.as_deref(), creds.as_ref(), None).await;
            });
        }));
    // Preflight the Node SDK bridge so a missing `npm install` (or `node`) surfaces at boot with a
    // fix command, instead of silently failing every turn at runtime.
    let bridge_path = sdk_runner::default_bridge_path();
    {
        let node_bin = std::env::var("AGENTIC_NODE_BIN").unwrap_or_else(|_| "node".into());
        for problem in sdk_runner::preflight(&node_bin, &bridge_path) {
            tracing::error!(target: "preflight", "{problem}");
        }
    }
    // Build the production title generator. Reads ANTHROPIC_BASE_URL /
    // ANTHROPIC_AUTH_TOKEN / ANTHROPIC_DEFAULT_HAIKU_MODEL from env; falls
    // back to ~/.claude/.credentials.json for auth if the env var is unset.
    // Fails fast at startup if neither auth source is available.
    let title_generator: std::sync::Arc<dyn crate::engine::title_client::TitleGenerator> =
        std::sync::Arc::new(
            crate::engine::title_client::AnthropicHttpTitleGenerator::from_env()
                .map_err(|e| format!("title generator init failed: {e:?}"))
                .expect("title generator init failed"),
        );
    let engine_cfg = EngineConfig {
        src_root: config.src_root.clone(),
        worktrees_root: config.worktrees_root.clone(),
        log_dir: config.log_dir.clone(),
        db_path: config.db_path.clone(),
        title_generator,
        retitle_enabled: config.retitle_enabled,
        max_concurrent: config.max_concurrent,
        git_org: config.git_org.clone(),
        claude_config_base: config.claude_config_base.clone(),
        // Injectable seams — Phase 5/6 will fill env-driven caps; pass None for now.
        clone_fn: None,
        sync_fn: None,
        // The per-turn transport: drive claude through the Agent SDK via the Node bridge. This is
        // what makes AskUserQuestion pause in-turn. Override the bridge path with AGENTIC_SDK_BRIDGE.
        runner: Some(Arc::new(sdk_runner::SdkRunner::new(bridge_path.clone()))),
        // Emit the engine's turn-lifecycle telemetry (turn_start/turn_result/turn_end —
        // queueWaitMs, ttftMs, costUsd, duration, concurrency) through tracing. Without this the
        // records are built and discarded without this hook.
        log_fn: Some(Arc::new(|rec: serde_json::Value| {
            let evt = rec
                .get("evt")
                .and_then(|v| v.as_str())
                .unwrap_or("event")
                .to_string();
            let sid = rec
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            tracing::info!(target: "engine", evt = %evt, session_id = %sid, record = %rec);
        })),
        now_fn: None,
        push_fn,                         // Phase 6: FCM push (wired above)
        usage_fn: None,                  // auto-resume probes the real OAuth usage endpoint
        idle_max_ms: config.idle_max_ms, // AGENTIC_TURN_IDLE_SEC
        wall_max_ms: config.wall_max_ms, // AGENTIC_TURN_WALL_SEC
        idle_ttl_ms: config.idle_ttl_ms, // AGENTIC_IDLE_TTL_SEC
        memory_max: config.memory_max.clone(),
        memory_high: config.memory_high.clone(),
        cpu_quota: config.cpu_quota.clone(),
        tasks_max: config.tasks_max.clone(),
    };
    // Use with_store so the engine shares the already-open Store + TranscriptCache.
    // This also runs recover() + reconcile_worktrees() via Engine::new semantics;
    // here we call with_store which skips auto-recover — call recover explicitly.
    let engine = Engine::with_store(engine_cfg, store.clone(), Some(transcript.clone()));
    engine.recover().await;
    engine.reconcile_worktrees().await;
    // Finalize delegate fan-outs interrupted by the prior restart (phantom "running" cards).
    engine.reconcile_delegate_runs().await;
    // Start the LiteLLM proxy sidecar so openai-protocol providers can run as delegate workers
    // (translates Anthropic↔OpenAI). No-op when litellm isn't installed.
    crate::engine::litellm::start_supervisor();
    // Keep the ChatGPT subscription access token fresh; reload the LiteLLM proxy on rotation so the
    // worker→proxy hop always carries a live bearer. No-op when not connected.
    tokio::spawn(async {
        loop {
            if let Ok(true) = tokio::task::spawn_blocking(|| {
                crate::engine::openai_oauth::refresh_if_needed(300)
            })
            .await
            .unwrap_or(Ok(false))
            {
                crate::engine::litellm::request_reload();
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
    let engine = Arc::new(engine);
    let engine_shutdown = engine.clone();
    let addr = format!("{}:{}", config.host, config.port);
    let state = AppState {
        config: config.clone(),
        throttle: Arc::new(Mutex::new(LoginThrottle::default())),
        store,
        transcript,
        engine,
        usage_cache: Arc::new(Mutex::new(state::UsageCache::default())),
        usage_inflight: Arc::new(tokio::sync::Mutex::new(())),
        usage_fn: None,
    };
    let make_service =
        api::app(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let tls_mode = TlsMode::from_config(&config);
    if TlsMode::byo_half_configured(&config) {
        tracing::warn!(
            "only one of AGENTIC_TLS_CERT / AGENTIC_TLS_KEY is set — a custom cert needs BOTH; using the auto self-signed cert instead"
        );
    }
    // Resolve to concrete cert/key PEM paths, generating + persisting the self-signed pair if needed.
    let tls_paths: Option<(std::path::PathBuf, std::path::PathBuf)> = match &tls_mode {
        TlsMode::Disabled => None,
        TlsMode::Byo { cert, key } => Some((cert.clone(), key.clone())),
        TlsMode::SelfSigned {
            dir,
            extra_sans,
            regen,
        } => match api::tls::ensure_self_signed(dir, extra_sans, *regen) {
            Ok(paths) => Some(paths),
            Err(e) => panic!(
                "could not create self-signed TLS cert in {}: {e}",
                dir.display()
            ),
        },
    };
    tracing::info!(
        "agentic-dev-server listening on {}://{addr} (src={}, maxConcurrent={}, tls={})",
        tls_mode.scheme(),
        config.src_root.display(),
        config
            .max_concurrent
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unlimited".into()),
        tls_mode.is_tls(),
    );
    if let Some((cert, _)) = tls_paths.as_ref() {
        match api::tls::cert_fingerprint_sha256(cert) {
            Ok(fp) => tracing::info!(
                target: "tls",
                "serving HTTPS with cert {} — SHA-256 {fp} (verify this when the app asks to trust it; GET /api/tls/cert.pem to download)",
                cert.display()
            ),
            Err(e) => {
                tracing::warn!(target: "tls", "could not fingerprint cert {}: {e}", cert.display())
            }
        }
    }

    match tls_paths {
        // Plain HTTP — AGENTIC_TLS=off. Historical path: bind a TcpListener + axum::serve.
        None => {
            let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
            axum::serve(listener, make_service)
                .with_graceful_shutdown(shutdown_signal(engine_shutdown))
                .await
                .expect("serve");
        }
        // HTTPS — BYO or self-signed cert, both loaded from PEM files and served via axum-server.
        Some((cert, key)) => {
            install_ring_provider();
            let socket_addr = resolve_addr(&addr).await;
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "load TLS cert {} / key {}: {e}",
                        cert.display(),
                        key.display()
                    )
                });
            let handle = axum_server::Handle::new();
            spawn_tls_shutdown(handle.clone(), engine_shutdown);
            axum_server::bind_rustls(socket_addr, tls)
                .handle(handle)
                .serve(make_service)
                .await
                .expect("serve tls");
        }
    }
}

/// Install the process-default rustls crypto provider (ring), matching the rest of the dependency
/// tree. axum-server's `from_pem_file` and rustls-acme both build their `ServerConfig` off this
/// process default, so it must be set before any TLS config is constructed. Idempotent.
fn install_ring_provider() {
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!(target: "tls", "rustls crypto provider already installed");
    }
}

/// Resolve the "host:port" bind string to a concrete `SocketAddr` for the axum-server (TLS) paths,
/// which bind a `SocketAddr` rather than a pre-made `TcpListener`. Falls back to DNS when the host
/// isn't a bare IP literal.
async fn resolve_addr(addr: &str) -> std::net::SocketAddr {
    if let Ok(a) = addr.parse::<std::net::SocketAddr>() {
        return a;
    }
    tokio::net::lookup_host(addr)
        .await
        .ok()
        .and_then(|mut it| it.next())
        .unwrap_or_else(|| panic!("cannot resolve bind address {addr}"))
}

/// Graceful shutdown for the axum-server (TLS) paths. Waits on the same SIGTERM/Ctrl-C signal used
/// by the plain-HTTP path — which also detaches in-flight turns via the engine — then drains the
/// server with a short grace period.
fn spawn_tls_shutdown(handle: axum_server::Handle<std::net::SocketAddr>, engine: Arc<Engine>) {
    tokio::spawn(async move {
        shutdown_signal(engine).await;
        handle.graceful_shutdown(Some(std::time::Duration::from_secs(3)));
    });
}

/// Log panics (incl. those inside spawned tasks) instead of letting them vanish. A panicking
/// pump/watchdog task otherwise disappears with no trace, leaking its concurrency slot.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".into());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("<non-string panic>");
        tracing::error!(target: "panic", location = %loc, "panic: {msg}");
        default_hook(info);
    }));
}

/// Resolve on SIGTERM (systemd/Docker stop) or Ctrl-C (SIGINT), then gracefully detach in-flight
/// turns via `close_with(false)` — the claude children keep running and writing their logs, to be
/// finalized by the next boot's `recover()`.
async fn shutdown_signal(engine: Arc<Engine>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!("failed to install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("shutdown signal received — detaching in-flight turns");
    engine.close_with(false);
}
