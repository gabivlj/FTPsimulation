use std::{
    collections::HashMap,
    fs::{self, File},
    io::Write,
    path::Path,
};

mod command;
#[macro_use]
pub mod config;
mod handler_read;
mod handler_write;
mod response;
use response::Response;
use user_manage::SystemUsers;

// use handlers::write_buffer_file_transfer;
use mio::net::{TcpListener, TcpStream};
use mio::{event::Event, Interest, Poll, Token, Waker};
use std::io::{Error, ErrorKind};
use std::net::Shutdown;
use std::sync::{Arc, Mutex};
use std::thread::spawn;

use crate::tcp::TCPImplementation;

use self::{handler_read::HandlerRead, handler_write::HandlerWrite};

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
    /// This kind of operation is when the server is saving a file from the client, Response is when there is a response, if there is none when closing, it assumes an error
    FileUpload(File, Option<Vec<u8>>),

    /// This kind of operation is when the server is serving a file to the client
    FileDownload(File),

    /// This kind of operation is when the server is just writing some data to the client
    Buffer(BufferToWrite),
}

pub enum RequestType {
    /// This request_type is only when we are instantly closing the connection after accepting it
    Closed(TcpStream),

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
    CommandTransfer(TcpStream, BufferToWrite, Option<Token>, Option<String>),

    /// This is the passive mode port that will accept connections
    /// It has a token where it references the CommandTransfer request_ctx
    PassiveModePort(TcpListener, Token),
}

pub struct RequestContext {
    pub request_type: RequestType,

    user_id: Option<String>,

    loged: bool,
}

impl RequestContext {
    fn new(request_type: RequestType) -> Self {
        Self {
            request_type,
            user_id: None,
            loged: false,
        }
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

    // Maximum connections
    max_connections: usize,

    // Current connections
    current_connections: usize,

    user_repository: Arc<Mutex<SystemUsers>>,
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
            max_connections: 50,
            current_connections: 0,
            actions: Arc::new(Mutex::new(Vec::new())),
            user_repository: Arc::new(Mutex::new(
                SystemUsers::load_data("./etc/users.json").expect("didn't work"),
            )),
        }
    }

    pub fn with_connection_capacity(max_connections: usize) -> Self {
        if !Path::new(ROOT).exists() {
            fs::create_dir(ROOT).expect("root dir hasn't been created");
        }
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            current_id: 0,
            max_connections,
            current_connections: 0,
            actions: Arc::new(Mutex::new(Vec::new())),
            user_repository: Arc::new(Mutex::new(
                SystemUsers::load_data("./etc/users.json").expect("didn't work"),
            )),
        }
    }

    fn add_connection(&mut self, token: Token, request_type: RequestType) {
        self.connections.lock().unwrap().insert(
            token,
            Arc::new(Mutex::new(RequestContext::new(request_type))),
        );
    }

    fn deregister(&self, poll: &Poll, rc: &mut RequestContext) -> Result<(), Error> {
        match &mut rc.request_type {
            RequestType::CommandTransfer(stream, _, _, _) => {
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

            RequestType::Closed(stream) => {
                poll.registry().deregister(stream)?;
            }
        }
        Ok(())
    }

    fn shutdown(rc: &mut RequestContext) -> Result<(), Error> {
        match &mut rc.request_type {
            RequestType::Closed(stream) => {
                let _ = stream.flush();
                stream.shutdown(Shutdown::Both)?;
            }
            RequestType::CommandTransfer(stream, _, _, _) => {
                let _ = stream.flush();
                stream.shutdown(Shutdown::Both)?;
            }

            RequestType::FileTransferActive(stream, _, _) => {
                let _ = stream.flush();
                stream.shutdown(Shutdown::Both)?;
            }

            RequestType::FileTransferPassive(stream, _, _) => {
                stream.shutdown(Shutdown::Both)?;
            }

            RequestType::PassiveModePort(_port, _) => {}
        }
        Ok(())
    }

    fn deregister_and_shutdown(&self, poll: &Poll, rc: &mut RequestContext) -> Result<(), Error> {
        let _ = self.deregister(poll, rc);
        FTPServer::shutdown(rc)?;
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
        print_stdout!(
            "[NEW_CONNECTION] {} - There is a brand new connection - Current connections: {} ",
            token.0,
            self.current_connections + 1
        );
        if self.max_connections <= self.current_connections {
            print_stdout!(
                "[NEW_CONNECTION] {} - Closing connection because it surpasses the maximum connections",
                token.0
            );
            poll.registry()
                .register(&mut stream, token, Interest::WRITABLE)?;
            self.add_connection(token, RequestType::Closed(stream));
            return Ok(());
        }
        self.current_connections += 1;
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
        print_stdout!("[WRITE_CONNECTION] - {} - Start Writing", token.0);
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
                print_stdout!("[WRITE_CONNECTION] - {} - Fatal error -> {}", token.0, err);
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
            print_stdout!("[WRITE_CONNECTION] - {} - Finished task", token.0);
        });
        Ok(())
    }

    fn read_connection(
        &mut self,
        poll: &Poll,
        waker: Arc<Waker>,
        event: &Event,
    ) -> Result<(), Error> {
        print_stdout!("[READ_CONNECTION] - {} - Start read", event.token().0);
        // Connection database reference
        let map_conn = self.connections.clone();
        let map_conn = map_conn.lock().unwrap();
        // Get request context
        let conn = {
            let connection = map_conn.get(&event.token()).ok_or(ErrorKind::NotFound)?;
            let arc = connection.clone();
            arc
        };
        let token = event.token();
        drop(map_conn);
        // Get the handler read component, basically in charge of reading and interpreting what is
        // getting sent by the client
        let mut handler_read = {
            let conn_ref = &mut conn.lock().unwrap();
            self.deregister(poll, conn_ref)?;
            HandlerRead::new(
                token,
                self.connections.clone(),
                conn.clone(),
                self.user_repository.clone(),
                conn_ref.user_id.clone(),
                conn_ref.loged,
            )
        };
        // Get action list mutex
        let actions = self.action_list();
        // Next connection ID if we accept a new connection
        let next_id = self.next_id();
        // Spawn thread
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
            let mut is_would_block = false;
            if let Err(err) = response.as_ref() {
                is_would_block = err.kind() == ErrorKind::WouldBlock;
            }
            let is_error_for_closing_connection = is_err && !is_would_block;
            if is_error_for_closing_connection {
                if let Err(err) = response {
                    print_stdout!(
                        "[READ_CONNECTION] - {} - Closing connection because error, {}",
                        token.0,
                        err
                    );
                    let _ = FTPServer::shutdown(&mut connection_mutex);
                    drop(connection_mutex);
                    let _ = waker.wake();
                }
            } else if is_would_block {
                drop(connection_mutex);
                print_stdout!("[READ_CONNECTION] - {} - Would block", token.0);
                let mut actions = actions.lock().unwrap();
                actions.push((
                    handler_read.connection_token,
                    connection_arc.clone(),
                    Interest::READABLE,
                ));
                let _ = waker.wake();
                drop(actions);
            } else {
                let callback = response.unwrap();
                // This means that the function needs to do additional stuff inside the `request_context`,
                // not the `request_type`
                if let Some(callback) = callback {
                    callback(&mut connection_mutex);
                }
                // Finally drop the mutex
                drop(connection_mutex);
                print_stdout!("[READ_CONNECTION] - {} - Adding actions", token.0);
                let mut actions = actions.lock().unwrap();
                for action in handler_read.actions {
                    actions.push(action);
                }
                drop(actions);
                let _ = waker.wake();
            }
            print_stdout!("[READ_CONNECTION] - {} - Finishing task", token.0);
        });
        Ok(())
    }

    /// This function should be called for almost every disconnection
    /// to do a proper cleanup everytime of every connection.
    /// If it returns an error it doesn't disconnect, useful when this is fired when read is closed but we are still reading
    /// something from the client. e.g let's say that the user sends a file, we are reading, and the user just closes when
    /// it just sent data, then this will be fired at the same time that the read is happening, that's why below you will
    /// see that if there is not a response yet from handle_read it means that it didn't finish reading!
    fn close_connection(
        &mut self,
        poll: &Poll,
        token: Token,
        waker: &Arc<Waker>,
    ) -> Result<(), Error> {
        print_stdout!("[CLOSE_CONNECTION] - {} - Closing connection", token.0);
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
        let mut conn = conn.lock().unwrap();
        let user_name = conn.user_id.clone();
        match &mut conn.request_type {
            RequestType::Closed(stream) => {
                let _ = poll.registry().deregister(stream);
                let _ = stream.flush();
                let _ = stream.shutdown(Shutdown::Both);
                print_stdout!(
                    "[CLOSE_CONNECTION] - {} - Closing connection because maximum connections reached",
                    token.0
                );
            }

            RequestType::FileTransferActive(stream, t, conn)
            | RequestType::FileTransferPassive(stream, t, conn) => {
                if let FileTransferType::FileUpload(_, data_to_be_sent) = t {
                    // As said in the function header, we shouldn't close this connection because
                    // we wanna keep reading
                    if data_to_be_sent.is_none() {
                        return Err(Error::from(ErrorKind::WriteZero));
                    }
                    let db = self.connections.clone();
                    let actions = self.actions.clone();
                    let conn = *conn;
                    let data = data_to_be_sent.clone().unwrap();
                    // We need the waker to send actions
                    let waker = waker.clone();
                    // Tell the command socket to send some stuff
                    spawn(move || {
                        print_stdout!(
                            "[CLOSE_CONNECTION] - {} - Closing connection File Upload - {}",
                            token.0,
                            std::str::from_utf8(&data).unwrap()
                        );
                        let db = db.lock().unwrap();
                        let command_conn = db.get(&conn)?;
                        let command_conn = command_conn.clone();
                        drop(db);
                        let mut actions = actions.lock().unwrap();
                        let mut cmd = command_conn.lock().unwrap();
                        if let RequestType::CommandTransfer(_, to_write, _, _) =
                            &mut cmd.request_type
                        {
                            to_write.reset(data);
                        }
                        drop(cmd);
                        actions.push((conn, command_conn, Interest::WRITABLE));
                        let _ = waker.wake();
                        Some(())
                    });
                }
                print_stdout!(
                    "[CLOSE_CONNECTION] - {} - Closing connection FTA or FTP",
                    token.0
                );
                let _ = poll.registry().deregister(stream);
                let _ = stream.flush();
                let _ = stream.shutdown(Shutdown::Both);
            }

            RequestType::CommandTransfer(stream, _, conn, _) => {
                print_stdout!(
                    "[CLOSE_CONNECTION] - {} - Closing connection command",
                    token.0
                );
                // Ignore error to be honest, don't care if we try to close twice
                let _ = poll.registry().deregister(stream);
                let _ = stream.flush();
                let _ = stream.shutdown(Shutdown::Both);
                let conn = conn.take();
                if let Some(user_name) = user_name {
                    let mut user_db = self.user_repository.lock().unwrap();
                    let u = user_db.get_user_mut(&user_name);
                    if let Some(user) = u {
                        let _ = user.change_dir("/");
                    }
                }

                if let Some(conn) = &conn {
                    let mut map_conn = map_conn_arc.lock().unwrap();
                    let connection = map_conn.get_mut(conn);
                    if let Some(connection) = connection {
                        print_stdout!(
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
                print_stdout!("[CLOSE_CONNECTION] - {} - Closing port", token.0);
                // We actually just deregister when we write
                poll.registry().deregister(stream)?;
            }
        }

        // Now delete it from the database
        if let Some(_) = self.connections.lock().unwrap().remove(&token) {
            print_stdout!("[CLOSE_CONNECTION] Successfully removing the connection.");
            if let RequestType::CommandTransfer(_, _, _, _) = &conn.request_type {
                self.current_connections -= 1;
            }
            print_stdout!(
                "[CLOSE_CONNECTION] Current control connections - {}",
                self.current_connections
            );
        }

        print_stdout!(
            "[CLOSE_CONNECTION] Current overall connections - {}",
            self.connections.lock().unwrap().len()
        );
        // Closing the connection, returning ok...
        Ok(())
    }
}

#[cfg(test)]
mod ftp_server_testing {
    use crate::port;
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

    fn log_in(stream: &mut TcpStream, username: &str, password: &str) {
        stream
            .write_all(&format!("USER {}\r\n", username).as_bytes())
            .expect("user login didn't work");
        expect_response(stream, "331 User name okay, need password.\r\n");
        stream
            .write_all(&format!("PASS {}\r\n", password).as_bytes())
            .expect("user login didn't work");
        expect_response(stream, "230 User logged in, proceed.\r\n");
    }

    use crate::system;

    #[test]
    fn it_works() {
        for _ in 0..100 {
            let result = TcpStream::connect("127.0.0.1:8080");
            if let Err(err) = result {
                panic!("{}", err);
            }
            let mut stream = result.unwrap();
            expect_response(&mut stream, "220 Service ready for new user.\r\n");
            log_in(&mut stream, "user_012", "123456");
            let srv = TcpListener::bind("127.0.0.1:2234").expect("to create server");
            // print_stdout!("expect writing everything");
            stream
                .write_all(&"PORT 127,0,0,1,8,186\r\n".as_bytes())
                .expect("writing everything");
            let join = std::thread::spawn(move || {
                // print_stdout!("accept conn");
                let (mut conn, _) = srv.accept().expect("expect to receive connection");
                let mut buff = [0; 1024];
                // print_stdout!("read 1st");
                let read = conn.read(&mut buff).expect("to have read");
                let v = system::ls("./root/user_012").unwrap();
                assert_eq!(v, &buff[..read]);
                // print_stdout!("read 2nd");
                let possible_err = conn.read(&mut buff);
                assert!(possible_err.unwrap() == 0);
            });
            // print_stdout!("Command okay");
            expect_response(&mut stream, "200 Command okay.\r\n");
            // print_stdout!("List");
            stream
                .write_all(&"LIST\r\n".as_bytes())
                .expect("writing everything");
            expect_response(
                &mut stream,
                "150 File status okay; about to open data connection.\r\n",
            );
            // print_stdout!("Closing");
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
            log_in(&mut stream, "user_test_it_works_2", "123456");
            let srv = TcpListener::bind("127.0.0.1:2235").expect("to create server");
            stream
                .write_all(&"PORT 127,0,0,1,8,187\r\n".as_bytes())
                .expect("writing everything");
            let join = std::thread::spawn(move || {
                let (mut conn, _) = srv.accept().expect("expect to receive connection");
                let mut buff = [0; 1024];
                // print_stdout!("read 1st");
                let read = conn.read(&mut buff).expect("to have read");
                let v = system::ls("./root/user_test_it_works_2").unwrap();
                assert_eq!(v, &buff[..read]);
                // print_stdout!("read 2nd");
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
            let mut stream = result.unwrap();
            expect_response(&mut stream, "220 Service ready for new user.\r\n");
            log_in(&mut stream, "user_test_it_works_3", "123456");
            let srv = TcpListener::bind("127.0.0.1:2232").expect("to create server");
            stream
                .write_all(&"PORT 127,0,0,1,8,184\r\n".as_bytes())
                .expect("writing everything");
            let join = std::thread::spawn(move || {
                let (mut conn, _) = srv.accept().expect("expect to receive connection");
                let mut buff = [0; 1024];
                let read = conn.read(&mut buff).expect("to have read");
                let v = system::ls("./root/user_test_it_works_3").unwrap();
                assert_eq!(v, &buff[..read]);
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
            stream
                .write_all(&"QUIT\r\n".as_bytes())
                .expect("writing everything");
            expect_response(&mut stream, "221 Service closing control connection.\r\n");
            join.join().unwrap();
            std::thread::sleep(Duration::from_millis(20));
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
        log_in(&mut stream, "user_test_image_transfer", "123456");
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

    #[test]
    fn image_transfer_02() {
        for _i in 0..100 {
            let result = TcpStream::connect("127.0.0.1:8080");
            if let Err(err) = result {
                panic!("{}", err);
            }
            let mut stream = result.unwrap();
            expect_response(&mut stream, "220 Service ready for new user.\r\n");
            log_in(&mut stream, "user_test_image_transfer_02", "123456");
            let srv = TcpListener::bind("127.0.0.1:2253").expect("to create server");
            stream
                .write_all(&"PORT 127,0,0,1,8,205\r\n".as_bytes())
                .expect("writing everything");
            upload_hello_world(srv, &mut stream);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn upload_active<'a>(stream: &mut TcpStream, to: &'a str, from: &'static str, port: u16) {
        let conn = format!("127.0.0.1:{}", port);
        let srv = TcpListener::bind(conn).expect("to create server");
        let (first, second) = port::get_ftp_port_pair(port);
        let command = format!("PORT 127,0,0,1,{},{}\r\n", first, second);
        stream
            .write_all(&command.as_bytes())
            .expect("writing everything");
        let join = std::thread::spawn(move || {
            let mut f = std::fs::OpenOptions::new().read(true).open(from).unwrap();
            let (mut conn, _) = srv.accept().expect("expect to receive connection");
            let mut buff = [0; 1024];
            loop {
                let r = f.read(&mut buff).expect("to have read");
                if r == 0 {
                    break;
                }
                let w = conn.write(&mut buff[0..r]).expect("to have read");
                assert!(w == r);
            }
            drop(f);
        });
        expect_response(stream, "200 Command okay.\r\n");
        let command = format!("STOR {}\r\n", to);
        stream
            .write_all(&command.as_bytes())
            .expect("writing everything");
        expect_response(
            stream,
            "150 File status okay; about to open data connection.\r\n",
        );
        expect_response(
            stream,
            "226 Closing data connection. Requested file action successful (for example, file transfer or file abort).\r\n",
        );
        join.join().unwrap();
    }

    fn recv_active<'a>(stream: &mut TcpStream, to: &'a str, from: &'static str, port: u16) {
        let conn = format!("127.0.0.1:{}", port);
        let srv = TcpListener::bind(conn).expect("to create server");
        let (first, second) = port::get_ftp_port_pair(port);
        let command = format!("PORT 127,0,0,1,{},{}\r\n", first, second);
        stream
            .write_all(&command.as_bytes())
            .expect("writing everything");
        let join = std::thread::spawn(move || {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .open(from)
                .unwrap();
            let (mut conn, _) = srv.accept().expect("expect to receive connection");
            let mut buff = [0; 1024];
            loop {
                let r = conn.read(&mut buff).expect("to have read");
                if r == 0 {
                    break;
                }
                let w = f.write(&mut buff[0..r]).expect("to have read");
                assert!(w == r);
            }
            drop(f);
        });
        expect_response(stream, "200 Command okay.\r\n");
        let command = format!("RETR {}\r\n", to);
        stream
            .write_all(&command.as_bytes())
            .expect("writing everything");
        expect_response(stream, "150 File download starts!\r\n");
        expect_response(
            stream,
            "226 Closing data connection. Requested file action successful. (file transfer)\r\n",
        );
        join.join().unwrap();
    }

    #[test]
    fn mkdir() {
        let result = TcpStream::connect("127.0.0.1:8080");
        let mut stream = result.unwrap();
        expect_response(&mut stream, "220 Service ready for new user.\r\n");
        log_in(&mut stream, "user_test_mkdir_01", "123456");
        stream
            .write_all(&"MKD /test\r\n".as_bytes())
            .expect("writing everything");
        expect_response(&mut stream, "257 'test' directory created.\r\n");
        stream
            .write_all(&"RMD /test\r\n".as_bytes())
            .expect("writing everything");
        expect_response(
            &mut stream,
            "250 Requested file action okay, completed.\r\n",
        );
    }

    fn upload_hello_world(srv: TcpListener, stream: &mut TcpStream) {
        let join = std::thread::spawn(move || {
            let (mut conn, _) = srv.accept().expect("expect to receive connection");
            for _i in 0..100 {
                let buff = b"Hello World!\n";
                conn.write_all(buff).expect("to have read");
            }
            let _ = conn.flush();
        });
        expect_response(stream, "200 Command okay.\r\n");
        stream
            .write_all(&"STOR ./thing.txt\r\n".as_bytes())
            .expect("writing everything");
        expect_response(
            stream,
            "150 File status okay; about to open data connection.\r\n",
        );
        expect_response(
            stream,
            "226 Closing data connection. Requested file action successful (for example, file transfer or file abort).\r\n",
        );
        join.join().unwrap();
    }

    #[test]
    fn cwd_test() {
        let result = TcpStream::connect("127.0.0.1:8080");
        let mut stream = result.unwrap();
        expect_response(&mut stream, "220 Service ready for new user.\r\n");
        log_in(&mut stream, "user_test_cwd_test", "123456");
        let srv = TcpListener::bind("127.0.0.1:2302").expect("to create server");
        stream
            .write_all(&"PORT 127,0,0,1,8,254\r\n".as_bytes())
            .expect("writing everything");
        upload_hello_world(srv, &mut stream);
        stream
            .write_all(&"MKD /test\r\n".as_bytes())
            .expect("writing everything");
        expect_response(&mut stream, "257 'test' directory created.\r\n");
        std::thread::sleep(Duration::from_micros(100));
        stream
            .write_all(&"CWD ./test\r\n".as_bytes())
            .expect("writing everything");
        expect_response(
            &mut stream,
            "250 Requested file action okay, completed.\r\n",
        );
        let srv = TcpListener::bind("127.0.0.1:2302").expect("to create server");
        stream
            .write_all(&"PORT 127,0,0,1,8,254\r\n".as_bytes())
            .expect("writing everything");
        upload_hello_world(srv, &mut stream);
        stream
            .write_all(&"DELE ../thing.txt\r\n".as_bytes())
            .expect("writing everything");
        expect_response(
            &mut stream,
            "250 Requested file action okay, completed.\r\n",
        );
        stream
            .write_all(&"CWD ../\r\n".as_bytes())
            .expect("writing everything");
        expect_response(
            &mut stream,
            "250 Requested file action okay, completed.\r\n",
        );
        stream
            .write_all(&"RMD ./test\r\n".as_bytes())
            .expect("writing everything");
        expect_response(
            &mut stream,
            "250 Requested file action okay, completed.\r\n",
        );
    }

    #[test]
    fn create_file_delete() {
        let result = TcpStream::connect("127.0.0.1:8080");
        let mut stream = result.unwrap();
        expect_response(&mut stream, "220 Service ready for new user.\r\n");
        log_in(&mut stream, "user_test_create_file_delete", "123456");
        let srv = TcpListener::bind("127.0.0.1:2303").expect("to create server");
        stream
            .write_all(&"PORT 127,0,0,1,8,255\r\n".as_bytes())
            .expect("writing everything");
        upload_hello_world(srv, &mut stream);
        stream
            .write_all(&"DELE ./thing.txt\r\n".as_bytes())
            .expect("writing everything");
        expect_response(
            &mut stream,
            "250 Requested file action okay, completed.\r\n",
        );
    }

    fn dele(stream: &mut TcpStream, what: &str) {
        let to_send = format!("{} {}\r\n", "DELE", what);
        stream
            .write_all(to_send.as_bytes())
            .expect("writing everything");
        expect_response(stream, "250 Requested file action okay, completed.\r\n");
    }

    fn cwd(stream: &mut TcpStream, str: &str) {
        let to_send = format!("{} {}\r\n", "CWD", str);
        stream
            .write_all(to_send.as_bytes())
            .expect("writing everything");
        expect_response(stream, "250 Requested file action okay, completed.\r\n");
    }

    fn rnto(stream: &mut TcpStream, from: &str, to: &str) {
        let from_cmd = format!("RNFR {}\r\n", from);
        let to_cmd = format!("RNTO {}\r\n", to);
        stream.write_all(from_cmd.as_bytes()).unwrap();
        expect_response(
            stream,
            "350 Requested file action pending further information.\r\n",
        );
        stream.write_all(to_cmd.as_bytes()).unwrap();
        expect_response(stream, "250 Requested file action okay, completed.\r\n");
    }

    fn pwd(stream: &mut TcpStream, expected: &str) {
        stream.write_all(&"PWD\r\n".as_bytes()).unwrap();
        expect_response(stream, format!("257 {}\r\n", expected).as_str());
    }

    use std::path::Path;

    fn mkd(stream: &mut TcpStream, path: &str) {
        let to_send = format!("{} {}\r\n", "MKD", path);
        stream
            .write_all(to_send.as_bytes())
            .expect("writing everything");
        let s = Path::new(path).file_name().unwrap().to_str().unwrap();
        let expected = format!("257 '{}' directory created.\r\n", s);
        expect_response(stream, &expected);
    }

    fn rmd(stream: &mut TcpStream, path: &str) {
        let to_send = format!("{} {}\r\n", "RMD", path);
        stream
            .write_all(to_send.as_bytes())
            .expect("writing everything");
        expect_response(stream, "250 Requested file action okay, completed.\r\n");
    }

    //cargo test --package ftp_server --bin ftp_server -- ftp::ftp_server_testing::pwd_test --exact --nocapture
    #[test]
    fn pwd_test() {
        let result = TcpStream::connect("127.0.0.1:8080");
        let mut stream = result.unwrap();
        expect_response(&mut stream, "220 Service ready for new user.\r\n");
        log_in(&mut stream, "user_pwd_test", "123456");
        pwd(&mut stream, "/");
        mkd(&mut stream, "/thing");
        mkd(&mut stream, "/thing/thing2");
        mkd(&mut stream, "/thing/thing2/thing3");
        cwd(&mut stream, "/thing");
        mkd(&mut stream, "./thing4");
        rmd(&mut stream, "./thing4");
        pwd(&mut stream, "/thing");
        cwd(&mut stream, "./thing2");
        pwd(&mut stream, "/thing/thing2");
        cwd(&mut stream, "/thing/thing2");
        pwd(&mut stream, "/thing/thing2");
        cwd(&mut stream, "../../");
        rmd(&mut stream, "/thing");
    }

    #[test]
    fn recv_test() {
        let result = TcpStream::connect("127.0.0.1:8080");
        let mut stream = result.unwrap();
        expect_response(&mut stream, "220 Service ready for new user.\r\n");
        log_in(&mut stream, "user_recv_test", "123456");
        upload_active(&mut stream, "./1.jpeg", "./test_files/1.jpeg", 1887);
        recv_active(&mut stream, "./1.jpeg", "./test_files/2.jpeg", 1887);
        dele(&mut stream, "/1.jpeg");
    }

    #[test]
    fn store_text_test() {
        let result = TcpStream::connect("127.0.0.1:8080");
        let mut stream = result.unwrap();
        expect_response(&mut stream, "220 Service ready for new user.\r\n");
        log_in(&mut stream, "user_store_text_test", "123456");
        upload_active(&mut stream, "./t.txt", "./test_files/hola.txt", 2777);
    }

    #[test]
    fn store_test() {
        let result = TcpStream::connect("127.0.0.1:8080");
        let mut stream = result.unwrap();
        expect_response(&mut stream, "220 Service ready for new user.\r\n");
        log_in(&mut stream, "user_store_test", "123456");
        mkd(&mut stream, "/thing");
        mkd(&mut stream, "/thing/thing2");
        upload_active(&mut stream, "./thing/1.jpeg", "./test_files/1.jpeg", 1777);
        upload_active(
            &mut stream,
            "./thing/thing2/1.jpeg",
            "./test_files/1.jpeg",
            1777,
        );
        rmd(&mut stream, "/thing");
    }

    #[test]
    fn store_2_test() {
        let result = TcpStream::connect("127.0.0.1:8080");
        let mut stream = result.unwrap();
        expect_response(&mut stream, "220 Service ready for new user.\r\n");
        log_in(&mut stream, "user_store_2_test", "123456");
        for i in 0..100 {
            let s = format!("./{}.jpeg", i);
            upload_active(&mut stream, &s, "./test_files/1.jpeg", 1778);
        }

        for i in 0..100 {
            let s = format!("./{}.jpeg", i);
            dele(&mut stream, &s);
        }
    }

    #[test]
    fn rnto_test() {
        let result = TcpStream::connect("127.0.0.1:8080");
        let mut stream = result.unwrap();
        expect_response(&mut stream, "220 Service ready for new user.\r\n");
        log_in(&mut stream, "user_rnto_test", "123456");
        pwd(&mut stream, "/");
        mkd(&mut stream, "/thing");
        mkd(&mut stream, "/thing/thing2");
        mkd(&mut stream, "/thing/thing2/thing3");
        rnto(&mut stream, "/thing", "/thing5");
        rnto(&mut stream, "/thing5/thing2", "/thing2");
        rmd(&mut stream, "/thing2");
        rmd(&mut stream, "/thing5");
    }

    #[test]
    fn delete_its_own_directory_test() {
        let result = TcpStream::connect("127.0.0.1:8080");
        let mut stream = result.unwrap();
        expect_response(&mut stream, "220 Service ready for new user.\r\n");
        log_in(&mut stream, "user_delete_its_own_directory_test", "123456");
        pwd(&mut stream, "/");
        mkd(&mut stream, "/thing");
        mkd(&mut stream, "/thing/thing2");
        mkd(&mut stream, "/thing/thing2/thing3");
        cwd(&mut stream, "/thing/thing2/thing3");
        rmd(&mut stream, "../thing3");
        pwd(&mut stream, "/thing/thing2");
        rmd(&mut stream, "../thing2");
        pwd(&mut stream, "/thing");
        rmd(&mut stream, "../thing");
        pwd(&mut stream, "/");
    }

    #[test]
    fn passive_connection() {
        // We could reduce these steps to functions and reuse them but its ok
        // at the moment
        let result = TcpStream::connect("127.0.0.1:8080");
        let mut stream = result.unwrap();
        expect_response(&mut stream, "220 Service ready for new user.\r\n");
        log_in(&mut stream, "user_test_image_transfer_02", "123456");
        stream.write_all(&"PASV\r\n".as_bytes()).unwrap();
        let mut b = BufReader::new(&mut stream);
        let mut str = String::new();
        b.read_line(&mut str).expect("to work");
        let end_no_jl = str.len() - 2;
        let s = &mut str[..end_no_jl - 1];
        let split = s.split('(').collect::<Vec<&str>>();
        let bytes: Vec<u8> = split
            .last()
            .unwrap()
            .split(',')
            .map(|el| el.parse().unwrap())
            .collect();
        let port: u16 = bytes[bytes.len() - 2] as u16 * 256 + bytes[bytes.len() - 1] as u16;
        let ip = format!(
            "{}.{}.{}.{}:{}",
            bytes[0], bytes[1], bytes[2], bytes[3], port
        );
        let mut connection =
            TcpStream::connect_timeout(ip.parse().as_ref().unwrap(), Duration::from_micros(1000))
                .unwrap();
        expect_response(&mut stream, "200 Command okay.\r\n");
        let join = std::thread::spawn(move || {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .open("./2.jpg")
                .unwrap();
            let mut buff = [0; 1024];
            loop {
                let read = connection.read(&mut buff).expect("to have read");
                if read == 0 {
                    break;
                }
                f.write(&buff[0..read]).expect("to work");
            }
        });
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
