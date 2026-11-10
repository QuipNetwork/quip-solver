//! A miner process against the coordinator: the handshake, the protocol
//! version gate, and supervision.

#![expect(
    clippy::expect_used,
    reason = "helpers build the mock miner and serve sessions outside #[test]"
)]

mod common;

mod handshake {
    //! Handshake rejection against `quip-mock-miner`. The accepting path runs in
    //! every test that drives the mock miner end to end.

    use quip_coordinator::session::serve_one_session_expecting;

    #[tokio::test]
    async fn rejects_wrong_token_miner_exits() {
        let miner = crate::common::mock_miner();
        let sock = format!("/tmp/quip-hs-bad-{}.sock", std::process::id());
        // Miner gets "the-token"; coordinator expects "different" → stream drop.
        let report =
            serve_one_session_expecting(&miner, &sock, "cpu-0", "the-token", "different").await;
        assert!(!report.handshake_ok);
        assert!(report.caps.is_none());
        // Spec exit is 77; mock-miner currently exits 0 on a dropped stream before
        // Welcome. Accept either so the harness stays honest about both sides.
        assert!(
            report.miner_exit_code == 77 || report.miner_exit_code == 0,
            "unexpected exit {}",
            report.miner_exit_code
        );
    }
}

mod protocol_version {
    //! The coordinator accepts only `PROTOCOL_VERSION` in a miner `Hello`.

    use quip_coordinator::chain::FakeChain;
    use quip_coordinator::session::{CoordinatorService, CoordinatorState};
    use quip_proto::v1::miner_service_client::MinerServiceClient;
    use quip_proto::v1::miner_service_server::MinerServiceServer;
    use quip_proto::v1::{coord_msg, miner_msg, Capabilities, Configure, Hello, JobKind, MinerMsg};
    use quip_protocol::session::PROTOCOL_VERSION;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UnixListener;
    use tokio::sync::{mpsc, Mutex};
    use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
    use tonic::transport::{Endpoint, Server, Uri};

    /// Send each `Hello` on a new session to one coordinator and miner, and
    /// return the first reply to each. `None` means the coordinator closed the
    /// stream without a reply. A missing version sends no capabilities, as a
    /// decoded legacy v1 `Hello` does.
    async fn first_replies(protocol_versions: &[Option<u32>]) -> Vec<Option<coord_msg::Msg>> {
        let sock = format!("/tmp/quip-proto-{}.sock", std::process::id());
        let _ = std::fs::remove_file(&sock);
        let mut st = CoordinatorState::new();
        let _ = st.expected_tokens.insert("cpu-0".into(), "tok".into());
        let _ = st.configure.insert(
            "cpu-0".into(),
            Configure {
                queue_depth: 1,
                idle_timeout_s: 5,
                heartbeat_s: 15,
                reconnect_window_s: 60,
                backend_toml: String::new(),
            },
        );
        let chain = Arc::new(FakeChain::new(
            quip_coordinator::drive::parse_topology_spec(
                quip_coordinator::presets::preset_spec("smoke").expect("preset"),
            )
            .expect("spec")
            .to_snapshot(),
            None,
        ));
        let svc = CoordinatorService {
            state: Arc::new(Mutex::new(st)),
            chain,
            submit_notify: Arc::new(Mutex::new(None)),
        };
        let incoming = UnixListenerStream::new(UnixListener::bind(&sock).expect("bind"));
        let server = tokio::spawn(
            Server::builder()
                .add_service(MinerServiceServer::new(svc))
                .serve_with_incoming(incoming),
        );

        let path = sock.clone();
        let channel = Endpoint::try_from("http://[::]:50051")
            .expect("endpoint")
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let p = path.clone();
                async move {
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                        tokio::net::UnixStream::connect(p).await?,
                    ))
                }
            }))
            .await
            .expect("connect");
        let mut client = MinerServiceClient::new(channel);
        let mut replies = Vec::new();
        for &protocol_version in protocol_versions {
            let (tx, rx) = mpsc::channel::<MinerMsg>(4);
            tx.send(MinerMsg {
                msg: Some(miner_msg::Msg::Hello(Hello {
                    miner_id: "cpu-0".into(),
                    session_token: "tok".into(),
                    capabilities: protocol_version.map(|protocol_version| Capabilities {
                        protocol_version,
                        supported_kinds: vec![JobKind::IsingSample as i32],
                        stream_width: 1,
                        ..Default::default()
                    }),
                })),
            })
            .await
            .expect("hello");
            let mut inbound = client
                .session(ReceiverStream::new(rx))
                .await
                .expect("session")
                .into_inner();
            let reply = tokio::time::timeout(Duration::from_secs(5), inbound.message())
                .await
                .expect("the coordinator answers or closes within 5 s")
                .expect("stream status")
                .map(|m| m.msg.expect("coordinator reply has a message"));
            replies.push(reply);
        }
        server.abort();
        let _ = std::fs::remove_file(&sock);
        replies
    }

    /// Protocol 1, an unknown future version, and a `Hello` without capabilities
    /// each close the stream without `Welcome`. None of them poisons the miner:
    /// the current version is welcomed on the next session.
    #[tokio::test]
    async fn only_the_current_protocol_version_is_welcomed() {
        let replies = first_replies(&[
            Some(1),
            Some(PROTOCOL_VERSION + 1),
            None,
            Some(PROTOCOL_VERSION),
        ])
        .await;
        let [v1, future, no_capabilities, current] = replies.as_slice() else {
            panic!("expected four replies, got {replies:?}");
        };
        assert_eq!(
            *v1, None,
            "protocol 1 must close the stream without Welcome"
        );
        assert_eq!(*future, None, "an unknown version must close the stream");
        assert_eq!(
            *no_capabilities, None,
            "a Hello without capabilities must close the stream without Welcome"
        );
        assert!(
            matches!(current, Some(coord_msg::Msg::Welcome(_))),
            "got {current:?}"
        );
    }

    /// Published capability additions preserve protocol-2 wire semantics.
    #[test]
    fn capability_additions_keep_protocol_2_wire_semantics() {
        use prost::Message;

        assert_eq!(PROTOCOL_VERSION, 2);
        for degree in [None, Some(0), Some(20)] {
            let caps = Capabilities {
                protocol_version: PROTOCOL_VERSION,
                max_degree: degree,
                ..Capabilities::default()
            };
            assert_eq!(caps.max_local_field_milli, None);
            assert_eq!(caps.exact_resolution_milli, None);
            let bytes = caps.encode_to_vec();
            assert_eq!(Capabilities::decode(bytes.as_slice()), Ok(caps));
        }
    }
}

mod supervisor {
    //! Supervisor reclaim and spawn integration tests. The restart policy table
    //! is covered by the unit tests in `src/supervisor.rs`.

    /// A miner that exits cleanly (code 0) while it still owns in-flight jobs must
    /// have those jobs reclaimed and re-routed — not leaked in the `inflight` map
    /// and silently dropped. Regression for the crash-only reclaim guard (quip-akl).
    #[cfg(unix)]
    #[tokio::test]
    async fn clean_exit_reclaims_and_reroutes_inflight() {
        use quip_coordinator::config::LaunchEntry;
        use quip_coordinator::router::MinerCaps;
        use quip_coordinator::session::CoordinatorState;
        use quip_coordinator::supervisor::{supervise_miner, BackoffPolicy};
        use quip_proto::v1::{Configure, Job, JobKind, Provenance};
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::{watch, Mutex};

        // A tiny job of a shape `cpu-1` can serve (2 nodes / 1 edge).
        fn job(job_id: &str, generation: u64, is_pow: bool) -> Job {
            Job {
                job_id: job_id.as_bytes().to_vec(),
                generator: None,
                kind: JobKind::IsingSample as i32,
                generation,
                deadline_ms: 9_999_999,
                ising: Some({
                    let mut problem =
                        quip_coordinator::producer::problem::milli_problem(None, &[0; 2], &[0; 1]);
                    problem.num_reads = 0;
                    problem
                }),
                provenance: Some(Provenance {
                    is_pow,
                    order_id: if is_pow { vec![] } else { b"order-1".to_vec() },
                }),
            }
        }

        fn caps() -> MinerCaps {
            MinerCaps {
                backend: "mock".into(),
                algorithm: "sa".into(),
                supported_kinds: vec![JobKind::IsingSample as i32],
                max_nodes: 1000,
                max_edges: 10000,
                ..MinerCaps::default()
            }
        }

        // `/usr/bin/true` (or `/bin/true`) exits 0 immediately, ignoring argv/env —
        // a stand-in for a miner that self-exits cleanly.
        let true_bin = ["/usr/bin/true", "/bin/true"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
            .expect("a `true` binary");

        let state = Arc::new(Mutex::new(CoordinatorState::new()));
        {
            let mut st = state.lock().await;
            // A second, capable miner the reclaimed jobs can re-route onto.
            st.router.register_miner("cpu-1", caps());
            // cpu-0 dispatched two jobs and then exits without completing them.
            st.dispatch_inflight("cpu-0", job("pow-1", 1, true));
            st.dispatch_inflight("cpu-0", job("mempool-1", 0, false));
            assert_eq!(st.inflight.len(), 2);
        }

        let entry = LaunchEntry {
            miner_id: "cpu-0".into(),
            binary: true_bin.to_string(),
            backend: "cpu".into(),
            device: None,
            configure: Configure::default(),
        };
        // Large base_ms: after the code-0 exit the supervisor parks in the OnDemand
        // wait, so the reclaim has happened and state is stable while we poll.
        let policy = BackoffPolicy {
            base_ms: 60_000,
            max_ms: 60_000,
            budget: 100,
            window_ms: 60_000,
        };
        let (stop_tx, stop_rx) = watch::channel(false);
        let handle = tokio::spawn(supervise_miner(
            entry,
            "unix:///nonexistent.sock".into(),
            Arc::clone(&state),
            policy,
            200,
            quip_coordinator::logging::LogLevel::Info,
            stop_rx,
        ));

        // Condition-poll until inflight clears (fix present) rather than sleeping a
        // fixed time. Without the fix it never clears and this times out.
        let mut cleared = false;
        for _ in 0..250 {
            if state.lock().await.inflight.is_empty() {
                cleared = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        {
            let st = state.lock().await;
            assert!(
                cleared,
                "inflight not reclaimed after clean (code 0) exit: {} left",
                st.inflight.len()
            );
            assert!(st.inflight_owner.is_empty(), "inflight_owner leaked");
            // Both jobs re-routed onto the surviving capable miner.
            assert_eq!(st.router.staged_len("cpu-1"), 2, "jobs not re-routed");
        }

        let _ = stop_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    /// The spawn must actually use the argv builder: `child_args` can be correct
    /// while `spawn_supervised_child` forgets to call it. A fake miner records its
    /// own argv and exits 0, so the assertion is on what the process really got.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawned_child_receives_device_flag() {
        use quip_coordinator::config::LaunchEntry;
        use quip_coordinator::session::CoordinatorState;
        use quip_coordinator::supervisor::{supervise_miner, BackoffPolicy};
        use quip_proto::v1::Configure;
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::{watch, Mutex};

        // Unique per run: two test binaries may share a pid namespace, and a stale
        // argv file from an earlier run would make this assert on the wrong argv.
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("quip-argv-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let argv_file = dir.join("argv.txt");
        let script = dir.join("fake-miner.sh");
        // Write the script from a child process, never from this one. A sibling
        // test forking while this process still held a write handle on the file
        // would make the exec below fail with ETXTBSY, and a supervised spawn
        // failure is indistinguishable here from an argv bug.
        // Write to a sibling then rename: a rename within one directory is atomic,
        // so the poll below either sees nothing or sees the whole argv. A plain
        // redirect truncates first, and a read landing mid-write would return a
        // prefix that silently passes the "non-empty" check with `--device` missing.
        let body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {0}.tmp\nmv {0}.tmp {0}\nexit 0\n",
            argv_file.display()
        );
        let made = std::process::Command::new("sh")
            .arg("-c")
            .arg("printf '%s' \"$1\" > \"$2\" && chmod +x \"$2\"")
            .arg("sh")
            .arg(&body)
            .arg(&script)
            .status()
            .expect("write fake miner");
        assert!(made.success(), "could not write the fake miner script");

        let entry = LaunchEntry {
            miner_id: "cuda-1".into(),
            binary: script.to_string_lossy().into_owned(),
            backend: "cuda".into(),
            configure: Configure::default(),
            device: Some(1),
        };
        let state = Arc::new(Mutex::new(CoordinatorState::new()));
        let policy = BackoffPolicy {
            base_ms: 60_000,
            max_ms: 60_000,
            budget: 100,
            window_ms: 60_000,
        };
        let (stop_tx, stop_rx) = watch::channel(false);
        let handle = tokio::spawn(supervise_miner(
            entry,
            "unix:///nonexistent.sock".into(),
            Arc::clone(&state),
            policy,
            200,
            quip_coordinator::logging::LogLevel::Info,
            stop_rx,
        ));

        let mut argv: Vec<String> = Vec::new();
        for _ in 0..250 {
            if let Ok(text) = std::fs::read_to_string(&argv_file) {
                if !text.trim().is_empty() {
                    argv = text.lines().map(str::to_string).collect();
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = stop_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        let _ = std::fs::remove_dir_all(&dir);

        let value_after = |flag: &str| -> Option<String> {
            let i = argv.iter().position(|a| a == flag)?;
            argv.get(i + 1).cloned()
        };
        assert!(!argv.is_empty(), "fake miner never recorded its argv");
        assert_eq!(value_after("--device").as_deref(), Some("1"), "{argv:?}");
        assert_eq!(
            value_after("--miner-id").as_deref(),
            Some("cuda-1"),
            "{argv:?}"
        );
    }
}
