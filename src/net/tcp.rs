use std::io;
use std::mem;
use std::net::SocketAddr;
use futures::{Poll, Async, Future, Stream};
use mio;

use fiber;
use io::poll;
use io::poll::EventedHandle;
use io::poll::SharableEvented;
use sync::oneshot::Monitor;
use super::{into_io_error, Bind};

/// A structure representing a socket server.
///
/// # Examples
///
/// ```
/// // See also: fibers/examples/tcp_example.rs
/// # extern crate fibers;
/// # extern crate futures;
/// use fibers::fiber::Executor;
/// use fibers::net::{TcpListener, TcpStream};
/// use fibers::sync::oneshot;
/// use futures::{Future, Stream};
///
/// # fn main() {
/// let mut executor = Executor::new().unwrap();
/// let (addr_tx, addr_rx) = oneshot::channel();
///
/// // Spawns TCP listener
/// executor.spawn(TcpListener::bind("127.0.0.1:0".parse().unwrap())
///     .and_then(|listener| {
///         let addr = listener.local_addr().unwrap();
///         println!("# Start listening: {}", addr);
///         addr_tx.send(addr).unwrap();
///         listener.incoming()
///             .for_each(move |(_client, addr)| {
///                 println!("# Accepted: {}", addr);
///                 Ok(())
///             })
///     })
///     .map_err(|e| panic!("{:?}", e)));
///
/// // Spawns TCP client
/// let mut monitor = executor.spawn_monitor(addr_rx.map_err(|e| panic!("{:?}", e))
///     .and_then(|server_addr| {
///         TcpStream::connect(server_addr).and_then(move |_stream| {
///             println!("# Connected: {}", server_addr);
///             Ok(())
///         })
///     }));
///
/// // Runs until the TCP client exits
/// while monitor.poll().unwrap().is_not_ready() {
///     executor.run_once(None).unwrap();
/// }
/// println!("# Succeeded");
/// # }
/// ```
#[derive(Debug)]
pub struct TcpListener {
    inner: SharableEvented<mio::tcp::TcpListener>,
    handle: EventedHandle,
    monitor: Option<Monitor<(), io::Error>>,
}
impl TcpListener {
    /// Makes a future to create a new `TcpListener` which will be bound to the specified address.
    pub fn bind(addr: SocketAddr) -> TcpListenerBind {
        TcpListenerBind(Bind::Bind(addr, mio::tcp::TcpListener::bind))
    }

    /// Makes a stream of the connections which will be accepted by this listener.
    pub fn incoming(self) -> Incoming {
        Incoming(self)
    }

    /// Returns the local socket address of this listener.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.with_inner_ref(|inner| inner.local_addr())
    }

    /// Calls `f` with the reference to the inner socket.
    pub unsafe fn with_inner<F, T>(&self, f: F) -> T
        where F: FnOnce(&mio::tcp::TcpListener) -> T
    {
        self.inner.with_inner_ref(f)
    }
}

/// A future which will create a new `TcpListener` which will be bound to the specified address.
///
/// This is created by calling `TcpListener::bind` function.
/// It is permitted to move the future across fibers.
///
/// # Panics
///
/// If the future is polled on the outside of a fiber, it may crash.
#[derive(Debug)]
pub struct TcpListenerBind(Bind<fn(&SocketAddr) -> io::Result<mio::tcp::TcpListener>,
                                mio::tcp::TcpListener>);
impl Future for TcpListenerBind {
    type Item = TcpListener;
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        Ok(self.0.poll()?.map(|(listener, handle)| {
            TcpListener {
                inner: listener,
                handle: handle,
                monitor: None,
            }
        }))
    }
}

/// An infinite stream of the connections which will be accepted by the listener.
///
/// This is created by calling `TcpListener::incoming` method.
/// It is permitted to move the future across fibers.
///
/// # Panics
///
/// If the stream is polled on the outside of a fiber, it may crash.
#[derive(Debug)]
pub struct Incoming(TcpListener);
impl Stream for Incoming {
    type Item = (Connected, SocketAddr);
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if let Some(mut monitor) = self.0.monitor.take() {
            if let Async::Ready(()) = monitor.poll().map_err(into_io_error)? {
                self.poll()
            } else {
                self.0.monitor = Some(monitor);
                Ok(Async::NotReady)
            }
        } else {
            match self.0.inner.with_inner_mut(|inner| inner.accept()) {
                Ok((stream, addr)) => {
                    let stream = SharableEvented::new(stream);
                    let register =
                        assert_some!(fiber::with_poller(|poller| poller.register(stream.clone())));
                    let stream = Connected(Some((stream, register)));
                    Ok(Async::Ready(Some((stream, addr))))
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        self.0.monitor = Some(self.0.handle.monitor(poll::Interest::Read));
                        Ok(Async::NotReady)
                    } else {
                        Err(e)
                    }
                }
            }
        }
    }
}

/// A future which represents a `TcpStream` connected to a `TcpListener`.
///
/// This is produced by `Incoming` stream.
/// It is permitted to move the future across fibers.
///
/// # Panics
///
/// If the future is polled on the outside of a fiber, it may crash.
#[derive(Debug)]
pub struct Connected(Option<(SharableEvented<mio::tcp::TcpStream>, poll::Register)>);
impl Future for Connected {
    type Item = TcpStream;
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let (stream, mut future) = self.0.take().expect("Cannot poll Connected twice");
        if let Async::Ready(handle) = future.poll().map_err(into_io_error)? {
            Ok(Async::Ready(TcpStream::new(stream, handle)))
        } else {
            self.0 = Some((stream, future));
            Ok(Async::NotReady)
        }
    }
}

/// A structure which represents a TCP stream between a local socket and a remote socket.
///
/// The socket will be closed when the value is dropped.
///
/// # Note
///
/// Non blocking mode is always enabled on this socket.
/// Roughly speaking, if an operation (read or write) for a socket would block,
/// it returns the `std::io::ErrorKind::WouldBlock` error and
/// current fiber is suspended until the socket becomes available.
/// If the fiber has multiple sockets (or other objects which may block),
/// it will be suspended only the case that all of them are unavailable.
///
/// To handle read/write operations over TCP streams in
/// [futures](https://github.com/alexcrichton/futures-rs) style,
/// it is preferred to use external crate like [handy_io](https://github.com/sile/handy_io).
///
/// # Examples
///
/// ```
/// // See also: fibers/examples/tcp_example.rs
/// # extern crate fibers;
/// # extern crate futures;
/// use fibers::fiber::Executor;
/// use fibers::net::{TcpListener, TcpStream};
/// use fibers::sync::oneshot;
/// use futures::{Future, Stream};
///
/// # fn main() {
/// let mut executor = Executor::new().unwrap();
/// let (addr_tx, addr_rx) = oneshot::channel();
///
/// // Spawns TCP listener
/// executor.spawn(TcpListener::bind("127.0.0.1:0".parse().unwrap())
///     .and_then(|listener| {
///         let addr = listener.local_addr().unwrap();
///         println!("# Start listening: {}", addr);
///         addr_tx.send(addr).unwrap();
///         listener.incoming()
///             .for_each(move |(_client, addr)| {
///                 println!("# Accepted: {}", addr);
///                 Ok(())
///             })
///     })
///     .map_err(|e| panic!("{:?}", e)));
///
/// // Spawns TCP client
/// let mut monitor = executor.spawn_monitor(addr_rx.map_err(|e| panic!("{:?}", e))
///     .and_then(|server_addr| {
///         TcpStream::connect(server_addr).and_then(move |mut stream| {
///             use std::io::Write;
///             println!("# Connected: {}", server_addr);
///             stream.write(b"Hello World!"); // This may return `WouldBlock` error
///             Ok(())
///         })
///     }));
///
/// // Runs until the TCP client exits
/// while monitor.poll().unwrap().is_not_ready() {
///     executor.run_once(None).unwrap();
/// }
/// println!("# Succeeded");
/// # }
/// ```
#[derive(Debug)]
pub struct TcpStream {
    inner: SharableEvented<mio::tcp::TcpStream>,
    handle: EventedHandle,
    read_monitor: Option<Monitor<(), io::Error>>,
    write_monitor: Option<Monitor<(), io::Error>>,
}
impl Clone for TcpStream {
    fn clone(&self) -> Self {
        TcpStream {
            inner: self.inner.clone(),
            handle: self.handle.clone(),
            read_monitor: None,
            write_monitor: None,
        }
    }
}
impl TcpStream {
    fn new(inner: SharableEvented<mio::tcp::TcpStream>, handle: EventedHandle) -> Self {
        TcpStream {
            inner: inner,
            handle: handle,
            read_monitor: None,
            write_monitor: None,
        }
    }

    /// Makes a future to open a TCP connection to a remote host.
    pub fn connect(addr: SocketAddr) -> Connect {
        Connect(ConnectInner::Connect(addr))
    }

    /// Returns the local socket address of this listener.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.with_inner_ref(|inner| inner.local_addr())
    }

    /// Returns the socket address of the remote peer of this TCP connection.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.inner.with_inner_ref(|inner| inner.peer_addr())
    }

    /// Calls `f` with the reference to the inner socket.
    pub unsafe fn with_inner<F, T>(&self, f: F) -> T
        where F: FnOnce(&mio::tcp::TcpStream) -> T
    {
        self.inner.with_inner_ref(f)
    }

    fn monitor(&mut self, interest: poll::Interest) -> &mut Option<Monitor<(), io::Error>> {
        if interest.is_read() {
            &mut self.read_monitor
        } else {
            &mut self.write_monitor
        }
    }
    fn operate<F, T>(&mut self, interest: poll::Interest, f: F) -> io::Result<T>
        where F: FnOnce(&mut mio::tcp::TcpStream) -> io::Result<T>
    {
        if let Some(mut monitor) = self.monitor(interest).take() {
            if let Async::Ready(()) =monitor.poll().map_err(into_io_error)? {
                self.operate(interest, f)
            } else {
                *self.monitor(interest) = Some(monitor);
                Err(mio::would_block())
            }
        } else {
            self.inner.with_inner_mut(|mut inner| f(&mut *inner)).map_err(|e| {
                if e.kind() == io::ErrorKind::WouldBlock {
                    *self.monitor(interest) = Some(self.handle.monitor(interest));
                }
                e
            })
        }
    }
}
impl io::Read for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.operate(poll::Interest::Read, |inner| inner.read(buf))
    }
}
impl io::Write for TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.operate(poll::Interest::Write, |inner| inner.write(buf))
    }
    fn flush(&mut self) -> io::Result<()> {
        self.operate(poll::Interest::Write, |inner| inner.flush())
    }
}

/// A future which will open a TCP connection to a remote host.
///
/// This is created by calling `TcpStream::connect` function.
/// It is permitted to move the future across fibers.
///
/// # Panics
///
/// If the future is polled on the outside of a fiber, it may crash.
#[derive(Debug)]
pub struct Connect(ConnectInner);
impl Future for Connect {
    type Item = TcpStream;
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll()
    }
}

#[derive(Debug)]
enum ConnectInner {
    Connect(SocketAddr),
    Registering(SharableEvented<mio::tcp::TcpStream>, poll::Register),
    Connecting(TcpStream),
    Polled,
}
impl Future for ConnectInner {
    type Item = TcpStream;
    type Error = io::Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match mem::replace(self, ConnectInner::Polled) {
            ConnectInner::Connect(addr) => {
                let stream = mio::tcp::TcpStream::connect(&addr)?;
                let stream = SharableEvented::new(stream);
                let register =
                    assert_some!(fiber::with_poller(|poller| poller.register(stream.clone())));
                *self = ConnectInner::Registering(stream, register);
                self.poll()
            }
            ConnectInner::Registering(stream, mut future) => {
                if let Async::Ready(handle) = future.poll().map_err(into_io_error)? {
                    *self = ConnectInner::Connecting(TcpStream::new(stream, handle));
                    self.poll()
                } else {
                    *self = ConnectInner::Registering(stream, future);
                    Ok(Async::NotReady)
                }
            }
            ConnectInner::Connecting(mut stream) => {
                use std::io::Write;
                match stream.flush() {
                    Ok(()) => Ok(Async::Ready(stream)),
                    Err(e) => {
                        if e.kind() == io::ErrorKind::WouldBlock {
                            Ok(Async::NotReady)
                        } else {
                            Err(e)
                        }
                    }
                }
            }
            ConnectInner::Polled => panic!("Cannot poll ConnectInner twice"),
        }
    }
}