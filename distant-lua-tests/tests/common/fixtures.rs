use distant_core::*;
use once_cell::sync::OnceCell;
use rstest::*;
use std::{net::SocketAddr, thread};
use tokio::{runtime::Runtime, sync::mpsc};

/// Context for some listening distant server
pub struct DistantServerCtx {
    pub addr: SocketAddr,
    pub key: String,
    done_tx: mpsc::Sender<()>,
}

impl DistantServerCtx {
    pub fn initialize() -> Self {
        let ip_addr = "127.0.0.1".parse().unwrap();
        let (done_tx, mut done_rx) = mpsc::channel(1);
        let (started_tx, mut started_rx) = mpsc::channel(1);

        // NOTE: We spawn a dedicated thread that runs our tokio runtime separately from our test
        // itself because using lua blocks the thread and prevents our runtime from working unless
        // we make the tokio test multi-threaded using `tokio::test(flavor = "multi_thread",
        // worker_threads = 1)` which isn't great because we're only using async tests for our
        // server itself; so, we hide that away since our test logic doesn't need to be async
        thread::spawn(move || match Runtime::new() {
            Ok(rt) => {
                rt.block_on(async move {
                    let opts = DistantServerOptions {
                        shutdown_after: None,
                        max_msg_capacity: 100,
                    };
                    let key = SecretKey::default();
                    let key_hex_string = key.unprotected_to_hex_key();
                    let codec = XChaCha20Poly1305Codec::from(key);
                    let (_server, port) =
                        DistantServer::bind(ip_addr, "0".parse().unwrap(), codec, opts)
                            .await
                            .unwrap();

                    started_tx.send(Ok((port, key_hex_string))).await.unwrap();

                    let _ = done_rx.recv().await;
                });
            }
            Err(x) => {
                started_tx.blocking_send(Err(x)).unwrap();
            }
        });

        // Extract our server startup data if we succeeded
        let (port, key) = started_rx.blocking_recv().unwrap().unwrap();

        Self {
            addr: SocketAddr::new(ip_addr, port),
            key,
            done_tx,
        }
    }
}

impl Drop for DistantServerCtx {
    /// Kills server upon drop
    fn drop(&mut self) {
        let _ = self.done_tx.send(());
    }
}

/// Returns a reference to the global distant server
#[fixture]
pub fn ctx() -> &'static DistantServerCtx {
    static CTX: OnceCell<DistantServerCtx> = OnceCell::new();

    CTX.get_or_init(DistantServerCtx::initialize)
}