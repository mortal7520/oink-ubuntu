//! A dedicated connected lookup must retain its peer's native UDP errors.
//! Shared endpoints keep Quinn's normal packet-loss/transient-error policy.
use quinn::{AsyncUdpSocket, UdpPoller, udp};
use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, ready},
};
use tokio::io::Interest;

pub(super) struct Socket {
    io: Arc<tokio::net::UdpSocket>,
    state: udp::UdpSocketState,
    readable: Mutex<Option<Writable>>,
}
impl std::fmt::Debug for Socket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinuxProbeSocket")
            .field("io", &self.io)
            .finish_non_exhaustive()
    }
}
impl Socket {
    pub(super) fn wrap(socket: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        // Only a connected socket can attribute an ICMP failure to this lookup.
        socket.peer_addr()?;
        let state = udp::UdpSocketState::new((&socket).into())?;
        Ok(Arc::new(Self {
            io: Arc::new(tokio::net::UdpSocket::from_std(socket)?),
            state,
            readable: Mutex::new(None),
        }))
    }
}

type Writable = Pin<Box<dyn Future<Output = io::Result<()>> + Send + Sync>>;
struct Poller {
    socket: Arc<Socket>,
    wait: Option<Writable>,
}
impl std::fmt::Debug for Poller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinuxProbePoller").finish()
    }
}
impl UdpPoller for Poller {
    fn poll_writable(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.wait.is_none() {
            let socket = self.socket.clone();
            self.wait = Some(Box::pin(async move { socket.io.writable().await }));
        }
        let result = ready!(self.wait.as_mut().expect("initialized").as_mut().poll(cx));
        self.wait = None;
        Poll::Ready(result)
    }
}
impl AsyncUdpSocket for Socket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(Poller {
            socket: self,
            wait: None,
        })
    }
    fn try_send(&self, transmit: &udp::Transmit<'_>) -> io::Result<()> {
        // UdpSocketState::send logs and discards non-WouldBlock errors. The
        // dedicated probe instead needs the original OS error, while retaining
        // Quinn's ECN/source-address/segmentation handling.
        self.io
            .try_io(Interest::WRITABLE, || self.state.try_send((&*self.io).into(), transmit))
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            // Linux may report ICMP as EPOLLERR without EPOLLIN. Tokio's
            // poll_recv_ready only waits for READABLE, so wait for ERROR too;
            // otherwise the first refusal is delayed until a retransmission.
            {
                let mut wait = self.readable.lock().expect("probe receive readiness");
                if wait.is_none() {
                    let io = self.io.clone();
                    *wait = Some(Box::pin(async move {
                        loop {
                            let ready = io.ready(Interest::READABLE | Interest::ERROR).await?;
                            if ready.is_error() {
                                // EPOLLERR may have no readable datagram. Read
                                // SO_ERROR directly on this connected socket.
                                if let Some(error) = io.take_error()? {
                                    return Err(error);
                                }
                                // A stale error readiness is not a datagram.
                                let _ = io.try_io(Interest::ERROR, || Err::<(), _>(io::ErrorKind::WouldBlock.into()));
                            }
                            if ready.is_readable() {
                                return Ok(());
                            }
                        }
                    }));
                }
                let result = ready!(wait.as_mut().expect("initialized").as_mut().poll(cx));
                *wait = None;
                result?;
            }
            match self
                .io
                .try_io(Interest::READABLE, || self.state.recv((&*self.io).into(), bufs, meta))
            {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                result => return Poll::Ready(result),
            }
        }
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
    }
    fn may_fragment(&self) -> bool {
        self.state.may_fragment()
    }
    fn max_transmit_segments(&self) -> usize {
        self.state.max_gso_segments()
    }
    fn max_receive_segments(&self) -> usize {
        self.state.gro_segments()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connected_datagrams_preserve_payload_and_receive_metadata() {
        // Manual lookups currently use an IPv4 socket (link::probe_socket).
        for address in ["127.0.0.1:0"] {
            let sender = std::net::UdpSocket::bind(address).unwrap();
            let receiver = std::net::UdpSocket::bind(address).unwrap();
            let source = sender.local_addr().unwrap();
            let destination = receiver.local_addr().unwrap();
            sender.connect(destination).unwrap();
            receiver.connect(source).unwrap();
            let sender = Socket::wrap(sender).unwrap();
            let receiver = Socket::wrap(receiver).unwrap();
            // Quinn may have multiple connection drivers waiting on one socket.
            let mut first = sender.clone().create_io_poller();
            let mut second = sender.clone().create_io_poller();
            for poller in [&mut first, &mut second] {
                std::future::poll_fn(|cx| poller.as_mut().poll_writable(cx))
                    .await
                    .unwrap();
            }
            for ecn in [None, Some(udp::EcnCodepoint::Ect0)] {
                sender
                    .try_send(&udp::Transmit {
                        destination,
                        ecn,
                        contents: b"probe",
                        segment_size: None,
                        src_ip: Some(destination.ip()),
                    })
                    .unwrap();
                let mut bytes = [0; 64];
                let mut bufs = [io::IoSliceMut::new(&mut bytes)];
                let mut meta = [udp::RecvMeta::default()];
                assert_eq!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(1),
                        std::future::poll_fn(|cx| receiver.poll_recv(cx, &mut bufs, &mut meta)),
                    )
                    .await
                    .unwrap()
                    .unwrap(),
                    1,
                );
                assert_eq!(&bytes[..meta[0].len], b"probe");
                assert_eq!(meta[0].addr, source);
                assert_eq!(meta[0].dst_ip, Some(destination.ip()));
                assert_eq!(meta[0].ecn, ecn);
            }
        }
    }

    #[tokio::test]
    async fn closed_connected_peer_reports_receive_error_without_retransmit() {
        let vacant = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let destination = vacant.local_addr().unwrap();
        // Allocate the sender while the destination is reserved, so the test
        // cannot accidentally create a self-connected UDP socket.
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.connect(destination).unwrap();
        drop(vacant);
        let sender = Socket::wrap(sender).unwrap();
        let mut poller = sender.clone().create_io_poller();
        std::future::poll_fn(|cx| poller.as_mut().poll_writable(cx))
            .await
            .unwrap();
        sender
            .try_send(&udp::Transmit {
                destination,
                ecn: None,
                contents: b"probe",
                segment_size: None,
                src_ip: None,
            })
            .unwrap();
        let mut bytes = [0; 64];
        let mut bufs = [io::IoSliceMut::new(&mut bytes)];
        let mut meta = [udp::RecvMeta::default()];
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            std::future::poll_fn(|cx| sender.poll_recv(cx, &mut bufs, &mut meta)),
        )
        .await
        .expect("a single send must surface the peer's ICMP error")
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert!(error.raw_os_error().is_some(), "the native OS error must be retained");
    }
}
