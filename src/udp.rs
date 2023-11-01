use anyhow::{anyhow, Context};
use futures_util::{stream, Stream};

use parking_lot::RwLock;
use pin_project::{pin_project, pinned_drop};
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::io::{Error, ErrorKind};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use log::warn;
use std::pin::{pin, Pin};
use std::sync::{Arc, Weak};
use std::task::{ready, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::futures::Notified;

use tokio::sync::Notify;
use tokio::time::{timeout, Interval};
use tracing::{debug, error, info};
use url::Host;

struct IoInner {
    has_data_to_read: Notify,
    has_read_data: Notify,
}
struct UdpServer {
    listener: Arc<UdpSocket>,
    peers: HashMap<SocketAddr, Arc<IoInner>, ahash::RandomState>,
    keys_to_delete: Arc<RwLock<Vec<SocketAddr>>>,
    cnx_timeout: Option<Duration>,
}

impl UdpServer {
    pub fn new(listener: Arc<UdpSocket>, timeout: Option<Duration>) -> Self {
        Self {
            listener,
            peers: HashMap::with_hasher(ahash::RandomState::new()),
            keys_to_delete: Default::default(),
            cnx_timeout: timeout,
        }
    }
    #[inline]
    fn clean_dead_keys(&mut self) {
        let nb_key_to_delete = self.keys_to_delete.read().len();
        if nb_key_to_delete == 0 {
            return;
        }

        debug!("Cleaning {} dead udp peers", nb_key_to_delete);
        let mut keys_to_delete = self.keys_to_delete.write();
        for key in keys_to_delete.iter() {
            self.peers.remove(key);
        }
        keys_to_delete.clear();
    }
    fn clone_socket(&self) -> Arc<UdpSocket> {
        self.listener.clone()
    }
}

#[pin_project(PinnedDrop)]
pub struct UdpStream {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    #[pin]
    watchdog_deadline: Option<Interval>,
    data_read_before_deadline: bool,
    has_been_notified: bool,
    #[pin]
    pending_notification: Option<Notified<'static>>,
    io: Arc<IoInner>,
    keys_to_delete: Weak<RwLock<Vec<SocketAddr>>>,
}

#[pinned_drop]
impl PinnedDrop for UdpStream {
    fn drop(self: Pin<&mut Self>) {
        if let Some(keys_to_delete) = self.keys_to_delete.upgrade() {
            keys_to_delete.write().push(self.peer);
        }

        // safety: we are dropping the notification as we extend its lifetime to 'static unsafely
        // So it must be gone before we drop its parent. It should never happen but in case
        let mut project = self.project();
        project.pending_notification.as_mut().set(None);
        project.io.has_read_data.notify_one();
    }
}

impl UdpStream {
    fn new(
        socket: Arc<UdpSocket>,
        peer: SocketAddr,
        watchdog_deadline: Option<Duration>,
        keys_to_delete: Weak<RwLock<Vec<SocketAddr>>>,
    ) -> (Self, Arc<IoInner>) {
        let has_data_to_read = Notify::new();
        let has_read_data = Notify::new();
        let io = Arc::new(IoInner {
            has_data_to_read,
            has_read_data,
        });
        let mut s = Self {
            socket,
            peer,
            watchdog_deadline: watchdog_deadline
                .map(|timeout| tokio::time::interval_at(tokio::time::Instant::now() + timeout, timeout)),
            data_read_before_deadline: false,
            has_been_notified: false,
            pending_notification: None,
            io: io.clone(),
            keys_to_delete,
        };

        let pending_notification = unsafe { std::mem::transmute(s.io.has_data_to_read.notified()) };
        s.pending_notification = Some(pending_notification);

        (s, io)
    }
}

impl AsyncRead for UdpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        obuf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut project = self.project();
        // Look that the timeout for client has not elapsed
        if let Some(mut deadline) = project.watchdog_deadline.as_pin_mut() {
            if deadline.poll_tick(cx).is_ready() {
                return if *project.data_read_before_deadline {
                    *project.data_read_before_deadline = false;
                    let _ = deadline.poll_tick(cx);
                    Poll::Pending
                } else {
                    Poll::Ready(Err(Error::new(
                        ErrorKind::TimedOut,
                        format!("UDP stream timeout with {}", project.peer),
                    )))
                };
            }
        }

        if let Some(notified) = project.pending_notification.as_mut().as_pin_mut() {
            ready!(notified.poll(cx));
            project.pending_notification.as_mut().set(None);
        }

        let peer = ready!(project.socket.poll_recv_from(cx, obuf))?;
        debug_assert_eq!(peer, *project.peer);
        *project.data_read_before_deadline = true;
        let notified: Notified<'static> = unsafe { std::mem::transmute(project.io.has_data_to_read.notified()) };
        project.pending_notification.as_mut().set(Some(notified));
        project.io.has_read_data.notify_one();
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for UdpStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> Poll<Result<usize, Error>> {
        self.socket.poll_send_to(cx, buf, self.peer)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Result<(), Error>> {
        self.socket.poll_send_ready(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }
}

pub async fn run_server(
    bind: SocketAddr,
    timeout: Option<Duration>,
) -> Result<impl Stream<Item = io::Result<UdpStream>>, anyhow::Error> {
    info!(
        "Starting UDP server listening cnx on {} with cnx timeout of {}s",
        bind,
        timeout.unwrap_or(Duration::from_secs(0)).as_secs()
    );

    let listener = UdpSocket::bind(bind)
        .await
        .with_context(|| format!("Cannot create UDP server {:?}", bind))?;

    let udp_server = UdpServer::new(Arc::new(listener), timeout);
    let stream = stream::unfold((udp_server, None), |(mut server, peer_with_data)| async move {
        // New returned peer hasn't read its data yet, await for it.
        if let Some(await_peer) = peer_with_data {
            if let Some(peer) = server.peers.get(&await_peer) {
                info!("waiting for peer {} to read its first data", await_peer.port());
                peer.has_read_data.notified().await;
                info!("peer {} to read its first data", await_peer.port());
            }
        };

        loop {
            server.clean_dead_keys();
            let peer_addr = match server.listener.peek_sender().await {
                Ok(ret) => ret,
                Err(err) => {
                    error!("Cannot read from UDP server. Closing server: {}", err);
                    return None;
                }
            };

            match server.peers.get(&peer_addr) {
                Some(io) => {
                    info!("waiting for peer {} to read its data", peer_addr.port());
                    io.has_data_to_read.notify_one();
                    io.has_read_data.notified().await;
                    info!("peer {} to read its data", peer_addr.port());
                }
                None => {
                    info!("New UDP connection from {}", peer_addr);
                    let (udp_client, io) = UdpStream::new(
                        server.clone_socket(),
                        peer_addr,
                        server.cnx_timeout,
                        Arc::downgrade(&server.keys_to_delete),
                    );
                    io.has_data_to_read.notify_waiters();
                    server.peers.insert(peer_addr, io);
                    return Some((Ok(udp_client), (server, Some(peer_addr))));
                }
            }
        }
    });

    Ok(stream)
}

#[derive(Clone)]
pub struct MyUdpSocket {
    socket: Arc<UdpSocket>,
}

impl MyUdpSocket {
    pub fn new(socket: Arc<UdpSocket>) -> Self {
        Self { socket }
    }
}

impl AsyncRead for MyUdpSocket {
    fn poll_read(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        unsafe { self.map_unchecked_mut(|x| &mut x.socket) }
            .poll_recv_from(cx, buf)
            .map(|x| x.map(|_| ()))
    }
}

impl AsyncWrite for MyUdpSocket {
    fn poll_write(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> Poll<Result<usize, Error>> {
        unsafe { self.map_unchecked_mut(|x| &mut x.socket) }.poll_send(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }
}

pub async fn connect(host: &Host<String>, port: u16, connect_timeout: Duration) -> anyhow::Result<MyUdpSocket> {
    info!("Opening UDP connection to {}:{}", host, port);

    let socket_addrs: Vec<SocketAddr> = match host {
        Host::Domain(domain) => timeout(connect_timeout, tokio::net::lookup_host(format!("{}:{}", domain, port)))
            .await
            .with_context(|| format!("cannot resolve domain: {}", domain))??
            .collect(),
        Host::Ipv4(ip) => vec![SocketAddr::V4(SocketAddrV4::new(*ip, port))],
        Host::Ipv6(ip) => vec![SocketAddr::V6(SocketAddrV6::new(*ip, port, 0, 0))],
    };

    let mut cnx = None;
    let mut last_err = None;
    for addr in socket_addrs {
        debug!("connecting to {}", addr);

        let socket = match &addr {
            SocketAddr::V4(_) => UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).await,
            SocketAddr::V6(_) => UdpSocket::bind(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0)).await,
        };

        let socket = match socket {
            Ok(socket) => socket,
            Err(err) => {
                warn!("cannot bind udp socket {:?}", err);
                continue;
            }
        };

        match timeout(connect_timeout, socket.connect(addr)).await {
            Ok(Ok(_)) => {
                cnx = Some(socket);
                break;
            }
            Ok(Err(err)) => {
                debug!("Cannot connect udp socket to specified peer {addr} reason {err}");
                last_err = Some(err);
            }
            Err(_) => {
                debug!(
                    "Cannot connect udp socket to specified peer {addr} due to timeout of {}s elapsed",
                    connect_timeout.as_secs()
                );
            }
        }
    }

    if let Some(cnx) = cnx {
        Ok(MyUdpSocket::new(Arc::new(cnx)))
    } else {
        Err(anyhow!("Cannot connect to udp peer {}:{} reason {:?}", host, port, last_err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{pin_mut, StreamExt};
    use tokio::io::AsyncReadExt;
    use tokio::time::error::Elapsed;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_udp_server() {
        let server_addr: SocketAddr = "[::1]:1234".parse().unwrap();
        let server = run_server(server_addr, None).await.unwrap();
        pin_mut!(server);

        // Should timeout
        let fut = timeout(Duration::from_millis(100), server.next()).await;
        assert!(matches!(fut, Err(Elapsed { .. })));

        // Send some data to the server
        let client = UdpSocket::bind("[::1]:0").await.unwrap();
        assert!(client.send_to(b"hello".as_ref(), server_addr).await.is_ok());

        // Should have a new connection
        let fut = timeout(Duration::from_millis(100), server.next()).await;
        assert!(matches!(fut, Ok(Some(Ok(_)))));

        // Should timeout again, no new client
        let fut2 = timeout(Duration::from_millis(100), server.next()).await;
        assert!(matches!(fut2, Err(Elapsed { .. })));

        // Take the stream of data
        let stream = fut.unwrap().unwrap().unwrap();
        pin_mut!(stream);

        let mut buf = [0u8; 25];
        let ret = stream.read(&mut buf).await;
        assert!(matches!(ret, Ok(5)));
        assert_eq!(&buf[..6], b"hello\0");

        assert!(client.send_to(b"world".as_ref(), server_addr).await.is_ok());
        assert!(client.send_to(b" test".as_ref(), server_addr).await.is_ok());

        // Server need to be polled to feed the stream with needed data
        let _ = timeout(Duration::from_millis(100), server.next()).await;
        // Udp Server should respect framing from the client and not merge the two packets
        let ret = timeout(Duration::from_millis(100), stream.read(&mut buf[5..])).await;
        assert!(matches!(ret, Ok(Ok(5))));

        let _ = timeout(Duration::from_millis(100), server.next()).await;
        let ret = timeout(Duration::from_millis(100), stream.read(&mut buf[10..])).await;
        assert!(matches!(ret, Ok(Ok(5))));
        assert_eq!(&buf[..16], b"helloworld test\0");
    }

    #[tokio::test]
    async fn test_multiple_client() {
        let server_addr: SocketAddr = "[::1]:1235".parse().unwrap();
        let mut server = Box::pin(run_server(server_addr, None).await.unwrap());

        // Send some data to the server
        let client = UdpSocket::bind("[::1]:0").await.unwrap();
        assert!(client.send_to(b"aaaaa".as_ref(), server_addr).await.is_ok());

        let client2 = UdpSocket::bind("[::1]:0").await.unwrap();
        assert!(client2.send_to(b"bbbbb".as_ref(), server_addr).await.is_ok());

        // Should have a new connection
        let fut = timeout(Duration::from_millis(100), server.next()).await;
        assert!(matches!(fut, Ok(Some(Ok(_)))));

        // Take the stream of data
        let stream = fut.unwrap().unwrap().unwrap();
        pin_mut!(stream);

        let mut buf = [0u8; 25];
        let ret = stream.read(&mut buf).await;
        assert!(matches!(ret, Ok(5)));
        assert_eq!(&buf[..6], b"aaaaa\0");

        // make the server make progress
        let fut2 = timeout(Duration::from_millis(100), server.next()).await;
        assert!(matches!(fut2, Ok(Some(Ok(_)))));

        let stream2 = fut2.unwrap().unwrap().unwrap();
        pin_mut!(stream2);

        // let the server make progress
        tokio::spawn(async move {
            loop {
                let _ = server.next().await;
            }
        });

        let ret = stream2.read(&mut buf).await;
        assert!(matches!(ret, Ok(5)));
        assert_eq!(&buf[..6], b"bbbbb\0");

        assert!(client.send_to(b"ccccc".as_ref(), server_addr).await.is_ok());
        assert!(client2.send_to(b"ddddd".as_ref(), server_addr).await.is_ok());
        assert!(client2.send_to(b"eeeee".as_ref(), server_addr).await.is_ok());
        assert!(client.send_to(b"fffff".as_ref(), server_addr).await.is_ok());

        let ret = stream.read(&mut buf).await;
        assert!(matches!(ret, Ok(5)));
        assert_eq!(&buf[..6], b"ccccc\0");

        let ret = stream2.read(&mut buf).await;
        assert!(matches!(ret, Ok(5)));
        assert_eq!(&buf[..6], b"ddddd\0");

        let ret = stream2.read(&mut buf).await;
        assert!(matches!(ret, Ok(5)));
        assert_eq!(&buf[..6], b"eeeee\0");

        let ret = stream.read(&mut buf).await;
        assert!(matches!(ret, Ok(5)));
        assert_eq!(&buf[..6], b"fffff\0");
    }

    #[tokio::test]
    async fn test_udp_should_timeout() {
        let server_addr: SocketAddr = "[::1]:1237".parse().unwrap();
        let socket_timeout = Duration::from_secs(1);
        let server = run_server(server_addr, Some(socket_timeout)).await.unwrap();
        pin_mut!(server);

        // Send some data to the server
        let client = UdpSocket::bind("[::1]:0").await.unwrap();
        assert!(client.send_to(b"hello".as_ref(), server_addr).await.is_ok());

        // Should have a new connection
        let fut = timeout(Duration::from_millis(100), server.next()).await;
        assert!(matches!(fut, Ok(Some(Ok(_)))));

        // Take the stream of data
        let stream = fut.unwrap().unwrap().unwrap();
        pin_mut!(stream);

        let mut buf = [0u8; 25];
        let ret = stream.read(&mut buf).await;
        assert!(matches!(ret, Ok(5)));
        assert_eq!(&buf[..6], b"hello\0");

        // Server need to be polled to feed the stream with need data
        let _ = timeout(Duration::from_millis(100), server.next()).await;
        let ret = timeout(Duration::from_millis(100), stream.read(&mut buf[5..])).await;
        assert!(ret.is_err());

        // Stream should be closed after the timeout
        tokio::time::sleep(socket_timeout).await;
        let ret = stream.read(&mut buf[5..]).await;
        assert!(ret.is_err());
    }
}
