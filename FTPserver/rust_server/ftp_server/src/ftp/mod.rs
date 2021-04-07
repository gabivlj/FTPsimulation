use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};

mod command;
mod handler_read;
mod handler_write;
mod response;
use command::Command;
use response::Response;

// use handlers::write_buffer_file_transfer;
use mio::net::{TcpListener, TcpStream};
use mio::{event::Event, Interest, Poll, Token, Waker};
use std::convert::TryFrom;
use std::io::{Error, ErrorKind};
use std::net::Shutdown;
use std::sync::{Arc, Mutex};
use std::thread::spawn;

use crate::tcp::TCPImplementation;

use self::{
    handler_read::HandlerRead,
    handler_write::{close_connection_recursive, HandlerWrite},
};

fn get_test_html(data: &str) -> Vec<u8> {
    return format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
        data.len(),
        data
    )
    .as_bytes()
    .to_vec();
}

fn create_response(response_code: Response, message: &str) -> Vec<u8> {
    format!("{} {}\r\n", response_code.0, message).into_bytes()
}

/// Buffer that is really useful to set to a writable request_context
pub struct BufferToWrite {
    /// Total data that this buffer is gonna send
    buffer: Vec<u8>,

    /// Current offset of the buffer
    offset: usize,

    /// We are using this callback mainly to do an action just after sending a command
    /// For example if we send a transition command 1XX, and make sure that just after that
    /// we start a file transfer, we need to pass a threadsafe callback that will start that action
    /// (For example starting a writable interest to the file transfer socket)
    /// Make sure that you use `.take()` for emptying the option
    callback_after_sending: Option<Box<dyn FnOnce() + Send>>,
}

impl BufferToWrite {
    fn default() -> Self {
        Self {
            buffer: Vec::default(),
            offset: 0,
            callback_after_sending: None,
        }
    }

    fn new(vector: Vec<u8>) -> Self {
        Self {
            buffer: vector,
            offset: 0,
            callback_after_sending: None,
        }
    }

    fn reset(&mut self, vector: Vec<u8>) {
        self.buffer = vector;
        self.offset = 0;
    }

    fn reset_str(&mut self, vector: &str) {
        self.buffer = vector.as_bytes().to_vec();
        self.offset = 0;
    }
}

// #[derive(Debug)]
pub enum FileTransferType {
    /// This kind of operation is when the server is saving a file from the client
    FileUpload(File),

    /// This kind of operation is when the server is serving a file to the client
    FileDownload(File),

    /// This kind of operation is when the server is just writing some data to the client
    Buffer(BufferToWrite),
}

/// We need to think about still
/// - storing user state (what do we need?)
/// - storing file state in file transfer
// TODO: Create user struct and all of that logic so we can keep a reference to a user in the request_context
// #[derive(Debug)]
pub enum RequestType {
    /// This requesst is a file transfer on passive mode.

    /// Also the token is for referencing the `CommandTransfer` req_ctx connection
    /// so we can send a command when the download is finished!
    FileTransferPassive(TcpStream, FileTransferType, Token),

    /// This requesst is a file transfer on active mode.    
    /// Also the token is for referencing the `CommandTransfer` req_ctx connection
    /// so we can send a command when the download is finished!
    FileTransferActive(TcpStream, FileTransferType, Token),

    /// TcpStream of the connection
    /// BufferToWrite is the buffer that is gonna be written on Write mode
    /// Option<Token> is the opened PassiveModePort/FileTransferActive/FileTransferPassive
    CommandTransfer(TcpStream, BufferToWrite, Option<Token>),

    /// This is the passive mode port that will accept connections
    /// It has a token where it references the CommandTransfer request_ctx
    PassiveModePort(TcpListener, Token),
}

pub struct RequestContext {
    pub request_type: RequestType,
    // (note): would be cool to have here the user_id reference when creating the user
    // socket_addr: SocketAddr,
}

impl RequestContext {
    fn new(request_type: RequestType) -> Self {
        Self { request_type }
    }
}

pub type RequestContextMutex = Arc<Mutex<RequestContext>>;

type Action = (Token, RequestContextMutex, Interest);

type ActionList = Arc<Mutex<Vec<Action>>>;

type HashMutex<K, V> = Arc<Mutex<HashMap<K, V>>>;

pub struct FTPServer {
    connections: HashMutex<Token, RequestContextMutex>,
    actions: ActionList,
    current_id: usize,
    port: usize,
}

pub const ROOT: &'static str = "./root";

impl FTPServer {
    pub fn new() -> Self {
        if !Path::new(ROOT).exists() {
            fs::create_dir(ROOT).expect("root dir hasn't been created");
        }
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            current_id: 0,
            port: 50_000,
            actions: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn add_connection(&mut self, token: Token, request_type: RequestType) {
        self.connections.lock().unwrap().insert(
            token,
            Arc::new(Mutex::new(RequestContext::new(request_type))),
        );
    }

    fn new_passive_listener(
        &mut self,
        poll: &Poll,
        command_transfer_conn: Token,
    ) -> Result<(), String> {
        let port = self.port;
        self.port += 1;
        let id = self.next_id();
        let mut listener = TcpListener::bind(
            format!("127.0.0.1:{}", port)
                .parse()
                .map_err(|_| format!("can't bind to this address"))?,
        )
        .map_err(|_| format!("can't bind to this port"))?;
        poll.registry()
            .register(&mut listener, Token(id), Interest::READABLE)
            .map_err(|_| format!("cannot register this socket"))?;
        self.add_connection(
            Token(id),
            RequestType::PassiveModePort(listener, command_transfer_conn),
        );
        Ok(())
    }

    fn deregister(&self, poll: &Poll, rc: &mut RequestContext) -> Result<(), Error> {
        match &mut rc.request_type {
            RequestType::CommandTransfer(stream, _, _) => {
                poll.registry().deregister(stream)?;
            }

            RequestType::FileTransferActive(stream, _, _) => {
                poll.registry().deregister(stream)?;
            }

            RequestType::FileTransferPassive(stream, _, _) => {
                poll.registry().deregister(stream)?;
            }

            RequestType::PassiveModePort(port, _) => {
                poll.registry().deregister(port)?;
            }
        }
        Ok(())
    }

    fn deregister_and_shutdown(&self, poll: &Poll, rc: &mut RequestContext) -> Result<(), Error> {
        let _ = self.deregister(poll, rc);
        match &mut rc.request_type {
            RequestType::CommandTransfer(stream, _, _) => {
                stream.flush();
                stream.shutdown(Shutdown::Both)?;
            }

            RequestType::FileTransferActive(stream, _, _) => {
                stream.flush();
                stream.shutdown(Shutdown::Both)?;
            }

            RequestType::FileTransferPassive(stream, _, _) => {
                stream.shutdown(Shutdown::Both)?;
            }

            RequestType::PassiveModePort(port, _) => {}
        }
        Ok(())
    }
}

impl TCPImplementation for FTPServer {
    fn action_list(&mut self) -> Arc<Mutex<Vec<Action>>> {
        self.actions.clone()
    }

    fn next_id(&mut self) -> usize {
        self.current_id += 1;
        self.current_id
    }

    fn new_connection(
        &mut self,
        _: Token,
        token: Token,
        poll: &Poll,
        mut stream: TcpStream,
    ) -> Result<(), std::io::Error> {
        println!("new connection!");
        poll.registry()
            .register(&mut stream, token, Interest::WRITABLE)?;
        self.add_connection(
            token,
            RequestType::CommandTransfer(
                stream,
                BufferToWrite::new(create_response(
                    Response::service_ready(),
                    "Service ready for new user.",
                )),
                None,
            ),
        );
        Ok(())
    }

    fn write_connection(
        &mut self,
        poll: &Poll,
        waker: Arc<Waker>,
        event: &Event,
    ) -> Result<(), Error> {
        let token = event.token();
        println!("[WRITE_CONNECTION] - {} - Start Writing", token.0);
        // TODO Make this a macro!
        let map_conn_arc = self.connections.clone();

        let map_conn = map_conn_arc.lock().unwrap();
        let connection = {
            let connection = map_conn.get(&token).ok_or(ErrorKind::NotFound)?;
            let arc = connection.clone();
            arc
        };
        drop(map_conn);
        let mut connection_mutex = connection.lock().unwrap();
        self.deregister(poll, &mut connection_mutex)?;
        drop(connection_mutex);
        let actions_ref = self.action_list();
        spawn(move || {
            let mut conn = connection.lock().unwrap();
            let mut handler = HandlerWrite::new(token, map_conn_arc.clone(), connection.clone());
            let write_result = handler.handle_write(&mut conn.request_type, &waker);
            if let Err(err) = &write_result {
                println!("[WRITE_CONNECTION] - {} - Fatal error -> {}", token.0, err);
                return;
            }
            // We drop the connection mutex here because we are promising the callback that it's 100% safe to take
            // any kind of mutex without getting a deadlock
            drop(conn);
            if let Some(write_callback) = write_result.unwrap() {
                write_callback();
            }
            let mut actions_locked = actions_ref.lock().unwrap();
            for action in handler.actions {
                actions_locked.push(action);
            }
            drop(actions_locked);
            let _ = waker.wake();
            println!("[WRITE_CONNECTION] - {} - Finished task", token.0);
        });
        Ok(())
    }

    fn read_connection(
        &mut self,
        poll: &Poll,
        waker: Arc<Waker>,
        event: &Event,
    ) -> Result<(), Error> {
        println!("[READ_CONNECTION] - {} - Start read", event.token().0);
        // first read
        let map_conn = self.connections.clone();
        let map_conn = map_conn.lock().unwrap();
        let conn = {
            let connection = map_conn.get(&event.token()).ok_or(ErrorKind::NotFound)?;
            let arc = connection.clone();
            arc
        };
        let token = event.token();
        drop(map_conn);
        self.deregister(poll, &mut conn.lock().unwrap())?;
        let mut handler_read = HandlerRead::new(token, self.connections.clone(), conn.clone());
        let actions = self.action_list();
        let next_id = self.next_id();
        spawn(move || {
            let connection_arc = conn.clone();
            let mut connection_mutex = connection_arc.lock().unwrap();
            let response = handler_read.handle_read(
                &mut connection_mutex.request_type,
                &waker,
                actions.clone(),
                next_id,
            );
            let is_err = response.is_err();

            // If it's a definitive error
            let is_would_block =
                is_err && response.as_ref().unwrap_err().kind() == ErrorKind::WouldBlock;
            let is_error_for_closing_connection = is_err && !is_would_block;
            if is_would_block {
                fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open("./debug.txt")
                    .unwrap()
                    .write(
                        format!(
                            "{:?} {:?}\n",
                            response,
                            handler_read
                                .actions
                                .iter()
                                .map(|e| e.2)
                                .collect::<Vec<Interest>>()
                        )
                        .as_bytes(),
                    )
                    .unwrap();
            }
            // TODO There is something strange when there is would block error,
            // ?? Investigate
            if is_error_for_closing_connection {
                println!(
                    "[READ_CONNECTION] - {} - Closing connection because error, {}",
                    token.0,
                    response.unwrap_err()
                );
                drop(connection_mutex);
                let _ = close_connection_recursive(
                    handler_read.connection_db.clone(),
                    handler_read.connection_token,
                );
                let _ = waker.wake();
            } else if is_would_block {
                drop(connection_mutex);
                println!("[READ_CONNECTION] - {} - Would block", token.0);
                let mut actions = actions.lock().unwrap();
                // The reason we are doing readable + writable is
                // that we don't know in a certain way the state of this socket!
                // Probably this event has been called just after deregistering
                actions.push((
                    handler_read.connection_token,
                    connection_arc.clone(),
                    Interest::READABLE,
                ));
                let _ = waker.wake();
                drop(actions);
            } else {
                drop(connection_mutex);
                println!("[READ_CONNECTION] - {} - Adding actions", token.0);
                let mut actions = actions.lock().unwrap();
                for action in handler_read.actions {
                    actions.push(action);
                }
                drop(actions);
                let _ = waker.wake();
            }
            println!("[READ_CONNECTION] - {} - Finishing task", token.0);
        });
        Ok(())
    }

    fn close_connection(&mut self, poll: &Poll, token: Token) -> Result<(), Error> {
        println!("[CLOSE_CONNECTION] - {} - Closing connection", token.0);
        let map_conn_arc = self.connections.clone();
        let map_conn = map_conn_arc.lock().unwrap();
        let conn = {
            let connection = map_conn.get(&token);
            if connection.is_none() {
                return Ok(());
            }
            let arc = connection.unwrap().clone();
            arc
        };
        drop(map_conn);
        self.connections.lock().unwrap().remove(&token);

        let mut conn = conn.lock().unwrap();
        match &mut conn.request_type {
            RequestType::FileTransferActive(stream, _, _)
            | RequestType::FileTransferPassive(stream, _, _) => {
                poll.registry().deregister(stream)?;
                stream.flush();
                stream.shutdown(Shutdown::Both)?;
                println!(
                    "[CLOSE_CONNECTION] - {} - Closing connection FTA or FTP",
                    token.0
                );
            }
            RequestType::CommandTransfer(stream, _, conn) => {
                println!(
                    "[CLOSE_CONNECTION] - {} - Closing connection command",
                    token.0
                );
                // Ignore error to be honest, don't care if we try to close twice
                let _ = poll.registry().deregister(stream);
                stream.flush();
                let _ = stream.shutdown(Shutdown::Both);
                let conn = conn.take();
                if let Some(conn) = &conn {
                    let mut map_conn = map_conn_arc.lock().unwrap();
                    let connection = map_conn.get_mut(conn);
                    if let Some(connection) = connection {
                        println!(
                            "[CLOSE_CONNECTION] - {} - Closing dangling connection",
                            token.0
                        );
                        let mut connection = connection.lock().unwrap();
                        // Don't care if we close twice
                        let _ = self.deregister_and_shutdown(poll, &mut connection);
                        drop(connection);
                        map_conn.remove(conn);
                    }
                }
            }
            RequestType::PassiveModePort(stream, _) => {
                println!("[CLOSE_CONNECTION] - {} - Closing port", token.0);
                // We actually just deregister when we write
                poll.registry().deregister(stream)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod ftp_server_testing {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::{io::Read, time::Duration};
    // use mio::net::{SocketAddr, TcpListener};

    fn expect_response(stream: &mut TcpStream, response_expects: &str) {
        // let mut buff = [0; 1024];
        let mut b = BufReader::new(stream);
        // let read = stream.read_until(&mut buff).expect("read didn't go well");
        // let str = std::str::from_utf8(&buff[0..read]).expect("error parsing response");
        let mut str = String::new();
        b.read_line(&mut str).expect("to work");
        assert_eq!(response_expects, str);
    }

    #[test]
    fn it_works() {
        for _ in 0..100 {
            let result = TcpStream::connect("127.0.0.1:8080");
            if let Err(err) = result {
                panic!("{}", err);
            }
            let mut stream = result.unwrap();
            expect_response(&mut stream, "220 Service ready for new user.\r\n");
            let srv = TcpListener::bind("127.0.0.1:2234").expect("to create server");
            // println!("expect writing everything");
            stream
                .write_all(&"PORT 127,0,0,1,8,186\r\n".as_bytes())
                .expect("writing everything");
            let join = std::thread::spawn(move || {
                // println!("accept conn");
                let (mut conn, _) = srv.accept().expect("expect to receive connection");
                let mut buff = [0; 1024];
                // println!("read 1st");
                let read = conn.read(&mut buff).expect("to have read");
                assert_eq!(read, 1000);
                assert_eq!(buff[0], 1);
                // println!("read 2nd");
                let possible_err = conn.read(&mut buff);
                assert!(possible_err.unwrap() == 0);
            });
            // println!("Command okay");
            expect_response(&mut stream, "200 Command okay.\r\n");
            // println!("List");
            stream
                .write_all(&"LIST\r\n".as_bytes())
                .expect("writing everything");
            expect_response(
                &mut stream,
                "150 File status okay; about to open data connection.\r\n",
            );
            // println!("Closing");
            expect_response(&mut stream, "226 Closing data connection. Requested file action successful (for example, file transfer or file abort).\r\n");
            join.join().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            let srv = TcpListener::bind("127.0.0.1:2234").expect("to create server");
            stream
                .write_all(&"PORT 127,0,0,1,8,186\r\n".as_bytes())
                .expect("writing everything");
            let join = std::thread::spawn(move || {
                let (mut conn, _) = srv.accept().expect("expect to receive connection");
                let mut buff = [0; 1024];
                let read = conn.read(&mut buff).expect("to have read");
                let expected = "Hello world!";
                assert_eq!(read, expected.len());
                assert_eq!(std::str::from_utf8(&buff[..read]).unwrap(), expected);
                let possible_err = conn.read(&mut buff);
                assert!(possible_err.unwrap() == 0);
            });
            expect_response(&mut stream, "200 Command okay.\r\n");
            stream
                .write_all(&"RETR ./testfile.txt\r\n".as_bytes())
                .expect("writing everything");
            expect_response(&mut stream, "150 File download starts!\r\n");
            join.join().unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn it_works2() {
        for _ in 0..100 {
            let result = TcpStream::connect("127.0.0.1:8080");
            if let Err(err) = result {
                panic!("{}", err);
            }
            let mut stream = result.unwrap();
            expect_response(&mut stream, "220 Service ready for new user.\r\n");
            let srv = TcpListener::bind("127.0.0.1:2235").expect("to create server");
            stream
                .write_all(&"PORT 127,0,0,1,8,187\r\n".as_bytes())
                .expect("writing everything");
            let join = std::thread::spawn(move || {
                let (mut conn, _) = srv.accept().expect("expect to receive connection");
                let mut buff = [0; 1024];

                let read = conn.read(&mut buff).expect("to have read");
                assert_eq!(read, 1000);
                assert_eq!(buff[0], 1);

                let possible_err = conn.read(&mut buff);
                assert!(possible_err.unwrap() == 0);
            });

            expect_response(&mut stream, "200 Command okay.\r\n");

            stream
                .write_all(&"LIST\r\n".as_bytes())
                .expect("writing everything");
            expect_response(
                &mut stream,
                "150 File status okay; about to open data connection.\r\n",
            );

            expect_response(&mut stream, "226 Closing data connection. Requested file action successful (for example, file transfer or file abort).\r\n");
            join.join().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            let srv = TcpListener::bind("127.0.0.1:2235").expect("to create server");
            stream
                .write_all(&"PORT 127,0,0,1,8,187\r\n".as_bytes())
                .expect("writing everything");
            let join = std::thread::spawn(move || {
                let (mut conn, _) = srv.accept().expect("expect to receive connection");
                let mut buff = [0; 1024];
                let read = conn.read(&mut buff).expect("to have read");
                let expected = "Hello world!";
                assert_eq!(read, expected.len());
                assert_eq!(std::str::from_utf8(&buff[..read]).unwrap(), expected);
                let possible_err = conn.read(&mut buff);
                assert!(possible_err.unwrap() == 0);
            });
            expect_response(&mut stream, "200 Command okay.\r\n");
            stream
                .write_all(&"RETR ./testfile.txt\r\n".as_bytes())
                .expect("writing everything");
            expect_response(&mut stream, "150 File download starts!\r\n");
            join.join().unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn it_works3() {
        for _ in 0..100 {
            let result = TcpStream::connect("127.0.0.1:8080");
            if let Err(err) = result {
                panic!("{}", err);
            }
            let mut stream = result.unwrap();
            expect_response(&mut stream, "220 Service ready for new user.\r\n");
            let srv = TcpListener::bind("127.0.0.1:2232").expect("to create server");
            stream
                .write_all(&"PORT 127,0,0,1,8,184\r\n".as_bytes())
                .expect("writing everything");
            let join = std::thread::spawn(move || {
                let (mut conn, _) = srv.accept().expect("expect to receive connection");
                let mut buff = [0; 1024];

                let read = conn.read(&mut buff).expect("to have read");
                assert_eq!(read, 1000);
                assert_eq!(buff[0], 1);

                let possible_err = conn.read(&mut buff);
                assert!(possible_err.unwrap() == 0);
            });

            expect_response(&mut stream, "200 Command okay.\r\n");

            stream
                .write_all(&"LIST\r\n".as_bytes())
                .expect("writing everything");
            expect_response(
                &mut stream,
                "150 File status okay; about to open data connection.\r\n",
            );

            expect_response(&mut stream, "226 Closing data connection. Requested file action successful (for example, file transfer or file abort).\r\n");
            join.join().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            // let srv = TcpListener::bind("127.0.0.1:2233").expect("to create server");
            // stream
            //     .write_all(&"PORT 127,0,0,1,8,185\r\n".as_bytes())
            //     .expect("writing everything");
            //     let join = std::thread::spawn(move || {
            //         let (mut conn, _) = srv.accept().expect("expect to receive connection");
            //         let mut buff = [0; 1024];
            //         let read = conn.read(&mut buff).expect("to have read");
            //         let expected = "Hello world!";
            //         assert_eq!(read, expected.len());
            //         assert_eq!(std::str::from_utf8(&buff[..read]).unwrap(), expected);
            //         let possible_err = conn.read(&mut buff);
            //         assert!(possible_err.unwrap() == 0);
            //     });
            //     expect_response(&mut stream, "200 Command okay.\r\n");
            //     stream
            //         .write_all(&"RETR ./testfile.txt\r\n".as_bytes())
            //         .expect("writing everything");
            //     expect_response(&mut stream, "150 File download starts!\r\n");
            //     join.join().unwrap();
            //     std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn image_transfer() {
        let result = TcpStream::connect("127.0.0.1:8080");
        if let Err(err) = result {
            panic!("{}", err);
        }
        let mut stream = result.unwrap();
        expect_response(&mut stream, "220 Service ready for new user.\r\n");
        let srv = TcpListener::bind("127.0.0.1:2233").expect("to create server");
        stream
            .write_all(&"PORT 127,0,0,1,8,185\r\n".as_bytes())
            .expect("writing everything");
        let join = std::thread::spawn(move || {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .open("./2.jpg")
                .unwrap();
            let (mut conn, _) = srv.accept().expect("expect to receive connection");
            let mut buff = [0; 1024];
            loop {
                let read = conn.read(&mut buff).expect("to have read");
                if read == 0 {
                    break;
                }
                f.write(&buff[0..read]).expect("to work");
            }
        });

        expect_response(&mut stream, "200 Command okay.\r\n");
        stream
            .write_all(&"RETR ./1.jpeg\r\n".as_bytes())
            .expect("writing everything");
        expect_response(&mut stream, "150 File download starts!\r\n");
        expect_response(
            &mut stream,
            "226 Closing data connection. Requested file action successful. (file transfer)\r\n",
        );
        join.join().unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
}
