use crate::{
    cli::opt::{CommonOpt, ConvertToIpAddrError, ListenSubcommand},
    core::{
        data::{Request, Response},
        net::{Transport, TransportReadHalf, TransportWriteHalf},
        session::Session,
        state::ServerState,
        utils,
    },
    ExitCode, ExitCodeError,
};
use derive_more::{Display, Error, From};
use fork::{daemon, Fork};
use log::*;
use orion::aead::SecretKey;
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    io,
    net::{tcp, TcpListener},
    runtime::Handle,
    sync::{mpsc, Mutex},
};

mod handler;

#[derive(Debug, Display, Error, From)]
pub enum Error {
    ConvertToIpAddrError(ConvertToIpAddrError),
    ForkError,
    IoError(io::Error),
}

impl ExitCodeError for Error {
    fn to_exit_code(&self) -> ExitCode {
        match self {
            Self::ConvertToIpAddrError(_) => ExitCode::NoHost,
            Self::ForkError => ExitCode::OsErr,
            Self::IoError(x) => x.to_exit_code(),
        }
    }
}

pub fn run(cmd: ListenSubcommand, opt: CommonOpt) -> Result<(), Error> {
    if cmd.daemon {
        // NOTE: We keep the stdin, stdout, stderr open so we can print out the pid with the parent
        match daemon(false, true) {
            Ok(Fork::Child) => {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(async { run_async(cmd, opt, true).await })?;
            }
            Ok(Fork::Parent(pid)) => {
                info!("[distant detached, pid = {}]", pid);
                if let Err(_) = fork::close_fd() {
                    return Err(Error::ForkError);
                }
            }
            Err(_) => return Err(Error::ForkError),
        }
    } else {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async { run_async(cmd, opt, false).await })?;
    }

    Ok(())
}

async fn run_async(cmd: ListenSubcommand, _opt: CommonOpt, is_forked: bool) -> Result<(), Error> {
    let addr = cmd.host.to_ip_addr(cmd.use_ipv6)?;
    let socket_addrs = cmd.port.make_socket_addrs(addr);
    let shutdown_after = cmd.to_shutdown_after_duration();

    // If specified, change the current working directory of this program
    if let Some(path) = cmd.current_dir.as_ref() {
        debug!("Setting current directory to {:?}", path);
        std::env::set_current_dir(path)?;
    }

    debug!("Binding to {} in range {}", addr, cmd.port);
    let listener = TcpListener::bind(socket_addrs.as_slice()).await?;

    let port = listener.local_addr()?.port();
    debug!("Bound to port: {}", port);

    // Print information about port, key, etc.
    let key = {
        let session = Session {
            host: "--".to_string(),
            port,
            auth_key: SecretKey::default(),
        };

        println!("{}", session.to_unprotected_string());

        Arc::new(session.into_auth_key())
    };

    // For the child, we want to fully disconnect it from pipes, which we do now
    if is_forked {
        if let Err(_) = fork::close_fd() {
            return Err(Error::ForkError);
        }
    }

    // Build our state for the server
    let state: Arc<Mutex<ServerState<SocketAddr>>> = Arc::new(Mutex::new(ServerState::default()));
    let (ct, notify) = utils::new_shutdown_task(Handle::current(), shutdown_after);

    // Wait for a client connection, then spawn a new task to handle
    // receiving data from the client
    loop {
        tokio::select! {
            result = listener.accept() => {match result {
                Ok((client, addr)) => {
                    // Establish a proper connection via a handshake, discarding the connection otherwise
                    let transport = match Transport::from_handshake(client, Some(Arc::clone(&key))).await {
                        Ok(transport) => transport,
                        Err(x) => {
                            error!("<Client @ {}> Failed handshake: {}", addr, x);
                            continue;
                        }
                    };

                    // Split the transport into read and write halves so we can handle input
                    // and output concurrently
                    let (t_read, t_write) = transport.into_split();
                    let (tx, rx) = mpsc::channel(cmd.max_msg_capacity as usize);
                    let ct_2 = Arc::clone(&ct);

                    // Spawn a new task that loops to handle requests from the client
                    tokio::spawn({
                        let f = request_loop(addr, Arc::clone(&state), t_read, tx);

                        let state = Arc::clone(&state);
                        async move {
                            ct_2.lock().await.increment();
                            f.await;
                            state.lock().await.cleanup_client(addr).await;
                            ct_2.lock().await.decrement();
                        }
                    });

                    // Spawn a new task that loops to handle responses to the client
                    tokio::spawn(async move { response_loop(addr, t_write, rx).await });
                }
                Err(x) => {
                    error!("Listener failed: {}", x);
                    break;
                }
            }}
            _ = notify.notified() => {
                warn!("Reached shutdown timeout, so terminating");
                break;
            }
        }
    }

    Ok(())
}

/// Repeatedly reads in new requests, processes them, and sends their responses to the
/// response loop
async fn request_loop(
    addr: SocketAddr,
    state: Arc<Mutex<ServerState<SocketAddr>>>,
    mut transport: TransportReadHalf<tcp::OwnedReadHalf>,
    tx: mpsc::Sender<Response>,
) {
    loop {
        match transport.receive::<Request>().await {
            Ok(Some(req)) => {
                debug!(
                    "<Client @ {}> Received request of type{} {}",
                    addr,
                    if req.payload.len() > 1 { "s" } else { "" },
                    req.to_payload_type_string()
                );

                if let Err(x) = handler::process(addr, Arc::clone(&state), req, tx.clone()).await {
                    error!("<Client @ {}> {}", addr, x);
                    break;
                }
            }
            Ok(None) => {
                info!("<Client @ {}> Closed connection", addr);
                break;
            }
            Err(x) => {
                error!("<Client @ {}> {}", addr, x);
                break;
            }
        }
    }
}

/// Repeatedly sends responses out over the wire
async fn response_loop(
    addr: SocketAddr,
    mut transport: TransportWriteHalf<tcp::OwnedWriteHalf>,
    mut rx: mpsc::Receiver<Response>,
) {
    while let Some(res) = rx.recv().await {
        if let Err(x) = transport.send(res).await {
            error!("<Client @ {}> {}", addr, x);
            break;
        }
    }
}
