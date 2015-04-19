//! HTTP Server
use std::fmt;
use std::io::{ErrorKind, BufWriter, Write};
use std::marker::PhantomData;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::thread::{self, JoinHandle};

use num_cpus;

pub use self::request::Request;
pub use self::response::Response;

pub use net::{Fresh, Streaming};

use HttpError::HttpIoError;
use {HttpResult};
use buffer::BufReader;
use header::{Headers, Connection, Expect};
use header::ConnectionOption::{Close, KeepAlive};
use method::Method;
use net::{NetworkListener, NetworkStream, HttpListener};
use status::StatusCode;
use uri::RequestUri;
use version::HttpVersion::{Http10, Http11};

use std::sync::{Arc, RwLock};

use self::listener::ListenerPool;

pub mod request;
pub mod response;

mod listener;

/// A server can listen on a TCP socket.
///
/// Once listening, it will create a `Request`/`Response` pair for each
/// incoming connection, and hand them to the provided handler.
#[derive(Debug)]
pub struct Server<'a, H: Handler<C>, C: ServerState, L = HttpListener> {
    handler: H,
    ssl: Option<(&'a Path, &'a Path)>,
    _marker: PhantomData<L>,
	state: Arc<RwLock<C>>
}

macro_rules! try_option(
    ($e:expr) => {{
        match $e {
            Some(v) => v,
            None => return None
        }
    }}
);

impl<'a, H: Handler<C>, C: ServerState, L: NetworkListener> Server<'a, H, C, L> {
    /// Creates a new server with the provided handler.
    pub fn new(handler: H, mut state: C) -> Server<'a, H, C, L> {
		state.reset();
        Server {
            handler: handler,
            ssl: None,
            _marker: PhantomData,
			state: Arc::new(RwLock::new(state))
        }
    }
}

impl<'a, H: Handler<C> + 'static, C: ServerState> Server<'a, H, C, HttpListener> {
    /// Creates a new server that will handle `HttpStream`s.
    pub fn http(handler: H, state: C) -> Server<'a, H, C, HttpListener> {
        Server::new(handler, state)
    }
    /// Creates a new server that will handler `HttpStreams`s using a TLS connection.
    pub fn https(handler: H, mut state: C, cert: &'a Path, key: &'a Path) -> Server<'a, H, C, HttpListener> {
		state.reset();
        Server {
            handler: handler,
            ssl: Some((cert, key)),
            _marker: PhantomData,
			state: Arc::new(RwLock::new(state))
        }
    }
}

impl<'a, H: Handler<C> + 'static, C: ServerState + 'static> Server<'a, H, C, HttpListener> {
    /// Binds to a socket, and starts handling connections using a task pool.
    pub fn listen_threads<T: ToSocketAddrs>(self, addr: T, threads: usize) -> HttpResult<Listening> {
        let listener = try!(match self.ssl {
            Some((cert, key)) => HttpListener::https(addr, cert, key),
            None => HttpListener::http(addr)
        });
        self.with_listener(listener, threads)
    }

    /// Binds to a socket and starts handling connections.
    pub fn listen<T: ToSocketAddrs>(self, addr: T) -> HttpResult<Listening> {
        self.listen_threads(addr, num_cpus::get() * 5 / 4)
    }
}
impl<
'a,
H: Handler<C> + 'static,
C: ServerState + 'static,
L: NetworkListener<Stream=S> + Send + 'static,
S: NetworkStream + Clone + Send> Server<'a, H, C, L> {
    /// Creates a new server that will handle `HttpStream`s.
    pub fn with_listener(self, mut listener: L, threads: usize) -> HttpResult<Listening> {
        let socket = try!(listener.local_addr());
        let handler = self.handler;
		let state = self.state;

        debug!("threads = {:?}", threads);
        let pool = ListenerPool::new(listener.clone());
        let work = move |mut stream| handle_connection(&mut stream, &handler, &state);
        let guard = thread::spawn(move || pool.accept(work, threads));

        Ok(Listening {
            _guard: Some(guard),
            socket: socket,
        })
    }
}

fn handle_connection<'h, S, C, H>(mut stream: &mut S, handler: &'h H, state: &Arc<RwLock<C>>)
where S: NetworkStream + Clone, H: Handler<C>, C: ServerState {
    debug!("Incoming stream");
    let addr = match stream.peer_addr() {
        Ok(addr) => addr,
        Err(e) => {
            error!("Peer Name error: {:?}", e);
            return;
        }
    };

    // FIXME: Use Type ascription
    let stream_clone: &mut NetworkStream = &mut stream.clone();
    let mut rdr = BufReader::new(stream_clone);
    let mut wrt = BufWriter::new(stream);

    let mut keep_alive = true;
    while keep_alive {
		let local_state = state.clone();

        let req = match Request::new(&mut rdr, addr) {
            Ok(req) => req,
            Err(HttpIoError(ref e)) if e.kind() == ErrorKind::ConnectionAborted => {
                trace!("tcp closed, cancelling keep-alive loop");
                break;
            }
            Err(HttpIoError(e)) => {
                debug!("ioerror in keepalive loop = {:?}", e);
                break;
            }
            Err(e) => {
                //TODO: send a 400 response
                error!("request error = {:?}", e);
                break;
            }
        };

        if req.version == Http11 && req.headers.get() == Some(&Expect::Continue) {
            let status = handler.check_continue((&req.method, &req.uri, &req.headers));
            match write!(&mut wrt, "{} {}\r\n\r\n", Http11, status) {
                Ok(..) => (),
                Err(e) => {
                    error!("error writing 100-continue: {:?}", e);
                    break;
                }
            }

            if status != StatusCode::Continue {
                debug!("non-100 status ({}) for Expect 100 request", status);
                break;
            }
        }

        keep_alive = match (req.version, req.headers.get::<Connection>()) {
            (Http10, Some(conn)) if !conn.contains(&KeepAlive) => false,
            (Http11, Some(conn)) if conn.contains(&Close)  => false,
            _ => true
        };
        let mut res = Response::new(&mut wrt);
        res.version = req.version;
        handler.handle(req, res, local_state);
        debug!("keep_alive = {:?}", keep_alive);
    }
}

/// A listening server, which can later be closed.
pub struct Listening {
    _guard: Option<JoinHandle<()>>,
    /// The socket addresses that the server is bound to.
    pub socket: SocketAddr,
}

impl fmt::Debug for Listening {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Listening {{ socket: {:?} }}", self.socket)
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        let _ = self._guard.take().map(|g| g.join());
    }
}

impl Listening {
    /// Stop the server from listening to its socket address.
    pub fn close(&mut self) -> HttpResult<()> {
        let _ = self._guard.take();
        debug!("closing server");
        //try!(self.acceptor.close());
        Ok(())
    }
}

/// A server state that is passed to the handling function.
/// The server state holds server information that is use for every request.
///
/// Meant to be used as a server cache that is only sometimes updated and can be used 
/// to store data such as server configuration, basic static sql query results, etc.
pub trait ServerState: Sync + Send {
	/// Reset the server state to a default state.
	/// Called on server start up and if a server state write operation becomes poisoned.
	fn reset(&mut self);
}

/// A handler that can handle incoming requests for a server.
pub trait Handler<C: ServerState>: Sync + Send {
    /// Receives a `Request`/`Response` pair, and should perform some action on them.
    ///
    /// This could reading from the request, and writing to the response.
    fn handle<'a, 'k>(&'a self, Request<'a, 'k>, Response<'a, Fresh>, Arc<RwLock<C>>);

    /// Called when a Request includes a `Expect: 100-continue` header.
    ///
    /// By default, this will always immediately response with a `StatusCode::Continue`,
    /// but can be overridden with custom behavior.
    fn check_continue(&self, _: (&Method, &RequestUri, &Headers)) -> StatusCode {
        StatusCode::Continue
    }
}

impl<C: ServerState, F> Handler<C> for F where F: Fn(Request, Response<Fresh>, Arc<RwLock<C>>), F: Sync + Send {
    fn handle<'a, 'k>(&'a self, req: Request<'a, 'k>, res: Response<'a, Fresh>, state: Arc<RwLock<C>>) {
        self(req, res, state)
    }
}

#[cfg(test)]
mod tests {
    use header::Headers;
    use method::Method;
    use mock::MockStream;
    use status::StatusCode;
    use uri::RequestUri;

    use super::{Request, Response, Fresh, Handler, handle_connection};

    #[test]
    fn test_check_continue_default() {
        let mut mock = MockStream::with_input(b"\
            POST /upload HTTP/1.1\r\n\
            Host: example.domain\r\n\
            Expect: 100-continue\r\n\
            Content-Length: 10\r\n\
            \r\n\
            1234567890\
        ");

        fn handle(_: Request, res: Response<Fresh>) {
            res.start().unwrap().end().unwrap();
        }

        handle_connection(&mut mock, &handle);
        let cont = b"HTTP/1.1 100 Continue\r\n\r\n";
        assert_eq!(&mock.write[..cont.len()], cont);
        let res = b"HTTP/1.1 200 OK\r\n";
        assert_eq!(&mock.write[cont.len()..cont.len() + res.len()], res);
    }

    #[test]
    fn test_check_continue_reject() {
        struct Reject;
        impl Handler for Reject {
            fn handle<'a, 'k>(&'a self, _: Request<'a, 'k>, res: Response<'a, Fresh>) {
                res.start().unwrap().end().unwrap();
            }

            fn check_continue(&self, _: (&Method, &RequestUri, &Headers)) -> StatusCode {
                StatusCode::ExpectationFailed
            }
        }

        let mut mock = MockStream::with_input(b"\
            POST /upload HTTP/1.1\r\n\
            Host: example.domain\r\n\
            Expect: 100-continue\r\n\
            Content-Length: 10\r\n\
            \r\n\
            1234567890\
        ");

        handle_connection(&mut mock, &Reject);
        assert_eq!(mock.write, &b"HTTP/1.1 417 Expectation Failed\r\n\r\n"[..]);
    }
}
