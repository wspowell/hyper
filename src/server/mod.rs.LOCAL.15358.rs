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
    pub fn listen_threads(self, ip: IpAddr, port: u16, threads: usize) -> HttpResult<Listening> {
        let addr = &(ip, port);
        let listener = try!(match self.ssl {
            Some((cert, key)) => HttpListener::https(addr, cert, key),
            None => HttpListener::http(addr)
        });
        self.with_listener(listener, threads)
    }

    /// Binds to a socket and starts handling connections.
    pub fn listen(self, ip: IpAddr, port: u16) -> HttpResult<Listening> {
        self.listen_threads(ip, port, os::num_cpus() * 5 / 4)
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
        let socket = try!(listener.socket_addr());
        let handler = self.handler;
		let state = self.state;

        debug!("threads = {:?}", threads);
        let pool = ListenerPool::new(listener.clone());
        let work = move |stream| keep_alive_loop(stream, &handler, &state);
        let guard = thread::scoped(move || pool.accept(work, threads));

        Ok(Listening {
            _guard: guard,
            socket: socket,
        })
    }
}

fn keep_alive_loop<'h, S, C, H>(mut stream: S, handler: &'h H, state: &Arc<RwLock<C>>)
where S: NetworkStream + Clone, H: Handler<C>, C: ServerState {
    debug!("Incoming stream");
    let addr = match stream.peer_addr() {
        Ok(addr) => addr,
        Err(e) => {
            error!("Peer Name error: {:?}", e);
            return;
        }
    };

    let mut stream_clone = stream.clone();
    let mut rdr = BufReader::new(&mut stream_clone as &mut NetworkStream);
    let mut wrt = BufWriter::new(stream);

    let mut keep_alive = true;
    while keep_alive {
		let local_state = state.clone();
        keep_alive = handle_connection(addr, &mut rdr, &mut wrt, handler, local_state);
        debug!("keep_alive = {:?}", keep_alive);
    }
}

fn handle_connection<'a, 'aa, 'h, S, H, C>(
    addr: SocketAddr,
    rdr: &'a mut BufReader<&'aa mut NetworkStream>,
    wrt: &mut BufWriter<S>,
    handler: &'h H,
	local_state: Arc<RwLock<C>>
) -> bool where 'aa: 'a, S: NetworkStream, H: Handler<C>, C: ServerState {
    let mut res = Response::new(wrt);
    let req = match Request::<'a, 'aa>::new(rdr, addr) {
        Ok(req) => req,
        Err(e@HttpIoError(_)) => {
            debug!("ioerror in keepalive loop = {:?}", e);
            return false;
        }
        Err(e) => {
            //TODO: send a 400 response
            error!("request error = {:?}", e);
            return false;
        }
    };

    let keep_alive = match (req.version, req.headers.get::<Connection>()) {
        (Http10, Some(conn)) if !conn.contains(&KeepAlive) => false,
        (Http11, Some(conn)) if conn.contains(&Close)  => false,
        _ => true
    };
    res.version = req.version;
    handler.handle(req, res, local_state);
    keep_alive
}

/// A listening server, which can later be closed.
pub struct Listening {
    _guard: JoinGuard<'static, ()>,
    /// The socket addresses that the server is bound to.
    pub socket: SocketAddr,
}

impl Listening {
    /// Stop the server from listening to its socket address.
    pub fn close(&mut self) -> HttpResult<()> {
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
    fn handle<'a, 'aa, 'b, 's>(&'s self, Request<'aa, 'a>, Response<'b, Fresh>, Arc<RwLock<C>>);
}

impl<C: ServerState, F> Handler<C> for F where F: Fn(Request, Response<Fresh>, Arc<RwLock<C>>), F: Sync + Send {
    fn handle<'a, 'aa, 'b, 's>(&'s self, req: Request<'a, 'aa>, res: Response<'b, Fresh>, state: Arc<RwLock<C>>) {
        self(req, res, state)
    }
}







