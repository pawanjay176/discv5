use crate::packet::*;
use crate::Executor;
use recv::*;
use send::*;
use std::net::SocketAddr;
use tokio::sync::{mpsc, oneshot};

mod filter;
mod recv;
mod send;

pub use filter::FilterConfig;
pub use recv::InboundPacket;
pub(crate) use recv::MAX_PACKET_SIZE;
pub use send::OutboundPacket;
/// Convenience objects for setting up the recv handler.
pub struct SocketConfig {
    /// The executor to spawn the tasks.
    pub executor: Option<Box<dyn Executor>>,
    /// The listening socket.
    pub socket_addr: SocketAddr,
    /// Configuration details for the packet filter.
    pub filter_config: FilterConfig,
    /// The WhoAreYou magic packet.
    pub whoareyou_magic: [u8; MAGIC_LENGTH],
}

/// Creates the UDP socket and handles the exit futures for the send/recv UDP handlers.
pub struct Socket {
    pub send: mpsc::Sender<OutboundPacket>,
    pub recv: mpsc::Receiver<InboundPacket>,
    sender_exit: oneshot::Sender<()>,
    recv_exit: oneshot::Sender<()>,
}

impl Socket {
    /// Creates a UDP socket, spawns a send/recv task and returns the channels.
    /// If this struct is dropped, the send/recv tasks will shutdown.
    pub(crate) fn new(config: &SocketConfig) -> Self {
        // set up the UDP socket
        let socket = {
            #[cfg(unix)]
            fn platform_specific(s: &net2::UdpBuilder) -> std::io::Result<()> {
                net2::unix::UnixUdpBuilderExt::reuse_port(s, true)?;
                Ok(())
            }
            #[cfg(not(unix))]
            fn platform_specific(_: &net2::UdpBuilder) -> std::io::Result<()> {
                Ok(())
            }
            let builder = net2::UdpBuilder::new_v4().expect("Could not setup UDP port");
            builder
                .reuse_address(true)
                .expect("Could not reuse address");
            platform_specific(&builder).expect("Failed to set platform");
            builder
                .bind(config.socket_addr)
                .expect("Could not bind to UDP socket")
        };
        let socket =
            tokio::net::UdpSocket::from_std(socket).expect("Could not instantiate UDP socket");

        // split the UDP socket
        let (recv_udp, send_udp) = socket.split();

        // spawn the recv handler
        let recv_config = RecvHandlerConfig {
            filter_config: config.filter_config.clone(),
            executor: config.executor.clone(),
            recv: recv_udp,
            whoareyou_magic: config.whoareyou_magic,
        };

        let (recv, recv_exit) = RecvHandler::spawn(recv_config);
        // spawn the sender handler
        let (send, sender_exit) = SendHandler::spawn(config.executor, send_udp);

        return Socket {
            send,
            recv,
            sender_exit,
            recv_exit,
        };
    }
}

impl std::ops::Drop for Socket {
    // close the send/recv handlers
    fn drop(&mut self) {
        let _ = self.sender_exit.send(());
        let _ = self.recv_exit.send(());
    }
}
