//! TCP server implementation suitable for chat rooms and games.
//!
//! The `lobby` crate is designed to provide a convenient way to spin up a server
//! which automatically keeps track of connected clients and provides facilities
//! for messaging them and polling them for data.
//!
//! Each client is represented using a unique integer id, and id's are recycled
//! as clients disconnect from the server.
//!
//! Here's how you spin up a `lobby` server and poll the clients for data:
//!
//! ```
//! extern crate lobby;
//! use lobby::{Lobby, ScanResult};
//!
//! let server = Lobby::new("127.0.0.1:8080").unwrap();
//!
//! loop {
//!     server.scan(|id, result| {
//!         let name = server.name(id).unwrap();
//!         match result {
//!             ScanResult::Connected => println!("{} has connected.", name),
//!             ScanResult::Data(data) => println!("{} sent {} bytes of data.", name, data.len()),
//!             ScanResult::IoError(err) => println!("{} ran into an IO error: {}", name, err),
//!             ScanResult::Disconnected => println!("{} has disconnected.", name),
//!         }
//!     });
//! }
//! ```
//!
//! Clients can then connect to the server using `TcpStream::connect()`. The first
//! thing a client should do after establishing the connection is send a UTF-8 encoded
//! name followed by a 0 byte to indicate its termination. After that, all further
//! data sent will be queued up to be scanned by the server.
#![feature(collections, io, net)]
#![allow(dead_code)]

extern crate vec_map;

use std::collections::VecDeque;
use std::io::{self, BufRead, Write, BufReader};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::thread::{self, JoinHandle};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Receiver, TryRecvError};

use vec_map::VecMap;

type ClientConn = (Receiver<io::Result<Vec<u8>>>, TcpStream);

/// A Lobby server instance.
pub struct Lobby {
    listener: TcpListener,
    connections: Arc<Mutex<VecMap<ClientConn>>>,
    names: Arc<Mutex<VecMap<String>>>,
    new_r: Receiver<usize>,
    thread: JoinHandle<()>,
}

impl Lobby {
    /// Create a new Lobby server at the specified address.
    ///
    /// Creating a Lobby will spawn a new thread listening for incoming connections,
    /// plus an additional thread for each connection. The first thing any new
    /// client should send is a UTF-8 encoded string followed by a 0 byte to indicate
    /// its termination, which will serve as the name associated with this connection.
    /// Note that this is not necessarily a unique identifier.
    ///
    /// Any additional data sent by the client will need to be processed via the `scan()`
    /// method.
    pub fn new<A>(addr: A) -> io::Result<Lobby> where A: ToSocketAddrs {
        let listener = try!(TcpListener::bind(&addr));
        let connections = Arc::new(Mutex::new(VecMap::new()));
        let names = Arc::new(Mutex::new(VecMap::new()));
        let (new_s, new_r) = channel();

        let thread = {
            let listener = listener.try_clone().unwrap();
            let connections = connections.clone();
            let names = names.clone();

            thread::spawn(move || {
                let mut id = 0;
                let free_ids = Arc::new(Mutex::new(VecDeque::new()));
                for conn in listener.incoming() {
                    if let Ok(conn) = conn {
                        let free_ids = free_ids.clone();
                        let new_id = match free_ids.lock().unwrap().pop_front() {
                            Some(id) => id,
                            None => { id += 1; id },
                        };

                        let conn_reader = conn.try_clone().unwrap();
                        let (ds, dr) = channel();
                        let new_s = new_s.clone();
                        let names = names.clone();

                        thread::spawn(move || {
                            let mut reader = BufReader::new(conn_reader);
                            let mut name_buf = Vec::new();
                            let my_id = new_id;

                            match reader.read_until(0, &mut name_buf) {
                                Ok(_) => {
                                    name_buf.pop(); // remove the delimiting 0
                                    names.lock().unwrap().insert(my_id, String::from_utf8(name_buf).unwrap());
                                    new_s.send(new_id).unwrap();
                                },
                                Err(_) => {
                                    drop(ds);
                                    free_ids.lock().unwrap().push_back(my_id);
                                    return;
                                },
                            }

                            loop {
                                let result = match reader.fill_buf() {
                                    Ok(data) if data.len() == 0 => Some(0),
                                    Ok(data) => { ds.send(Ok(data.to_vec())).unwrap(); Some(data.len()) },
                                    Err(e) => { ds.send(Err(e)).unwrap(); None },
                                };

                                if let Some(read) = result {
                                    if read > 0 {
                                        reader.consume(read);
                                    } else {
                                        drop(ds);
                                        free_ids.lock().unwrap().push_back(my_id);
                                        break;
                                    }
                                }
                            }
                        });

                        connections.lock().unwrap().insert(new_id, (dr, conn));
                    }
                }
            })
        };

        Ok(Lobby{
            listener: listener,
            connections: connections,
            names: names,
            new_r: new_r,
            thread: thread,
        })
    }

    /// Send a message to all connected clients.
    ///
    /// Returns a list of tuples pairing the id of each client that ran into an IO error with
    /// the error itself.
    pub fn message_all(&self, data: &[u8]) -> Vec<(usize, io::Error)> {
        self.message(|_| true, data)
    }

    /// Send a message to a single client.
    ///
    /// Returns a list of tuples pairing the id of each client that ran into an IO error with
    /// the error itself.
    pub fn message_client(&self, client: usize, data: &[u8]) -> Vec<(usize, io::Error)> {
        self.message(|id| id == client, data)
    }

    /// Send a message to every client but one. Useful for, e.g., one client messaging the others.
    ///
    /// Returns a list of tuples pairing the id of each client that ran into an IO error with
    /// the error itself.
    pub fn message_rest(&self, client: usize, data: &[u8]) -> Vec<(usize, io::Error)> {
        self.message(|id| id != client, data)
    }

    /// Send a message to every connected client for which `predicate` returns true.
    ///
    /// Returns a list of tuples pairing the id of each client that ran into an IO error with
    /// the error itself.
    pub fn message<P>(&self, predicate: P, data: &[u8]) -> Vec<(usize, io::Error)> where P: Fn(usize) -> bool {
        let mut failed = Vec::new();
        for (id, &mut (_, ref mut conn)) in self.connections.lock().unwrap().iter_mut().filter(|&(id, _)| predicate(id)) {
            if let Err(e) = conn.write_all(data) {
                failed.push((id, e));
            }
        }
        failed
    }

    /// Scan the clients' message queues for data.
    ///
    /// Note that the callback is only invoked if there is something to report, and that
    /// this method does not block. Most applications will want to wrap this call up
    /// in their main loop in order to continuously process data.
    pub fn scan<F: Fn(usize, ScanResult) -> ()>(&self, callback: F) {
        loop {
            match self.new_r.try_recv() {
                Ok(id) => callback(id, ScanResult::Connected),
                Err(e) if e == TryRecvError::Empty => break,
                Err(e) if e == TryRecvError::Disconnected => {
                    panic!("tried to check for new clients on disconnected channel!");
                },
                Err(_) => unimplemented!(),
            }
        }

        let mut results = Vec::with_capacity(self.connections.lock().unwrap().len());

        for (id, &mut (ref mut dr, _)) in self.connections.lock().unwrap().iter_mut() {
            match dr.try_recv() {
                Ok(Ok(data)) => results.push((id, ScanResult::Data(data))),
                Ok(Err(err)) => results.push((id, ScanResult::IoError(err))),
                Err(TryRecvError::Empty) => {}, // do nothing
                Err(TryRecvError::Disconnected) => results.push((id, ScanResult::Disconnected)),
            }
        }

        for (id, result) in results.into_iter() {
            if let ScanResult::Disconnected = result {
                self.connections.lock().unwrap().remove(id);
            }
            callback(id, result);
        }
    }

    /// Get the registered name for a given client.
    pub fn name(&self, client: usize) -> Option<String> {
        self.names.lock().unwrap().get(client).map(|s| s.clone())
    }
}

/// The result of a client scan.
pub enum ScanResult {
    /// The client sent data.
    Data(Vec<u8>),
    /// An IO error occurred while scanning.
    IoError(io::Error),
    /// A new client has connected.
    Connected,
    /// A client has disconnected.
    Disconnected,
}
